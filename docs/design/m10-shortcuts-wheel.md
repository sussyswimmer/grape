# M10 design: the Shortcuts wheel

Status: design for `DECISIONS.md` D47, revised after three design reviews (macOS feasibility, controller, contract). macOS first; Windows gets stubs now and the real thing later. Nothing here has run on a Mac.

This note is the contract the code is written against, the way `m1-drag-wheel.md` is for the drag wheels. Behavior lives in `INTERACTION_SPEC.md` §12; what was learned from the reference app lives in `RADIAL_FINDINGS.md`. Where this note and those disagree, they win and this note gets fixed.

## 1. What it is

A fourth wheel that needs no drag. The user presses a global hotkey, the wheel opens at the cursor, and each segment is a **shortcut** the user made in Settings. A shortcut is a name, an icon and one or more **steps** (press a key combination, type text, open something, run a script, move a window, …), or it is a **group** that opens a second ring of shortcuts. Which **menu** shows depends on the app in front: an app's own menu if it has one, otherwise the **Everywhere** menu.

Words: *Shortcuts wheel*, *shortcut*, *step*, *group*, *menu*, *Everywhere*. These are Grape's words. Don't borrow the reference app's names for things.

Out of scope for this change (listed in ROADMAP M10 as unchecked items): AI steps and an online preset library (both need the network: golden rule 3), opening the wheel from a hot corner, a shake, a mouse button or a bare modifier, an app switcher, visual effects, appearance options, preset files, media keys, a keydown recorder for chords, a check against the system's own shortcuts, and everything on Windows.

## 2. Where the code goes

| Piece | Where | Notes |
|---|---|---|
| Keys and chords (pure) | `crates/grape-platform/src/keys.rs` | `KeyCode`, `Chord`, the reserved list. No serde (the crate has none). |
| Portable entry points | `crates/grape-platform/src/shortcuts.rs` | Same shape as `file_actions.rs`: `pub fn` → `imp::`. Also the pure `placement_frame`. |
| macOS | `crates/grape-platform/src/macos/{hotkey.rs,input.rs,shortcuts.rs}` | Carbon hot keys, CGEvent posting, AX window moves, pickers. |
| macOS pure rules | `crates/grape-platform/src/mac_rules.rs` | Virtual key table, Carbon and CGEvent masks, program builders. Only `macos/*` may call it. |
| Windows, other | `windows/mod.rs`, `unsupported.rs` | Every new `imp::` function returns `PlatformError::Unsupported`. |
| Scripted twins | `crates/grape-platform/src/mock.rs` | Hotkey and pointer scripts for the controller's tests. |
| Event types | `apps/desktop/src-tauri/src/events.rs` | `UiMode`, `UiContent::Shortcuts`, `CloseReason::Picked`. |
| Model (pure) | `apps/desktop/src-tauri/src/shortcuts/model.rs` | Menus, shortcuts, steps, limits, validation, menu choice, text variables. |
| File | `apps/desktop/src-tauri/src/shortcuts/store.rs` | `shortcuts.json`: load (total), save (atomic), move aside. |
| Wheel session (pure) | `apps/desktop/src-tauri/src/shortcuts/session.rs` | The state in §5. Driven by `controller.rs`. |
| Runner | `apps/desktop/src-tauri/src/shortcuts/run.rs` | Runs steps in order behind an `Actuator` trait. |
| Commands, state, wiring | `apps/desktop/src-tauri/src/shortcuts/mod.rs` | `ShortcutsState`, the `shortcuts_*` commands, the hotkey binding. |
| Overlay | `apps/desktop/ui/src/wheel/*`, `shared/events.ts` | A new content kind. Rust still owns the hit test. |
| Settings | `apps/desktop/ui/src/settings/shortcuts-{model,view}.ts` | Its own root in `settings.html`, mounted by `src/settings.ts`. |

Rules of the layout:
- `grape-core` doesn't change. `hit_test` and `wheel_layout` take only a count; the session reads `GEOMETRY.inner_r` and `outer_r` too.
- Nothing reachable from `keys.rs`, from `shortcuts.rs`' pure functions or from the app may reference `mac_rules`: that module only exists on macOS and under `cfg(test)`.
- No new package enters `Cargo.lock`. Carbon, ApplicationServices and the keyboard-layout calls are declared by hand; `objc2-core-graphics` and `objc2-foundation` only gain features.

## 3. Keys (`grape-platform::keys`)

```rust
/// A key that isn't a modifier, named by its W3C `KeyboardEvent.code` (a place on the keyboard).
pub enum KeyCode { KeyA..KeyZ, Digit0..Digit9, F1..F20, Space, Enter, Tab, Escape,
    Backspace, Delete, ArrowUp, ArrowDown, ArrowLeft, ArrowRight, Home, End, PageUp, PageDown,
    Minus, Equal, BracketLeft, BracketRight, Backslash, Semicolon, Quote, Comma, Period, Slash, Backquote }

impl KeyCode {
    pub const ALL: &'static [KeyCode];
    pub fn id(self) -> &'static str;              // "KeyA", "Digit4", "F5", "ArrowUp"
    pub fn from_id(id: &str) -> Option<KeyCode>;  // exact match only
    pub fn label(self) -> &'static str;           // "A", "4", "F5", "↑", "Space", "Return"… (the US layout's names)
}

/// A key with the modifiers held with it.
pub struct Chord { pub key: KeyCode, pub mods: Modifiers }

pub enum HotkeyProblem { NeedsModifier, Reserved }
impl Chord {
    /// Why this chord can't open the wheel, if it can't.
    pub fn hotkey_problem(self, mac: bool) -> Option<HotkeyProblem>;
    /// "Control+Option+G" on the Mac, "Ctrl+Alt+G" elsewhere. Order: ctrl, alt, shift, meta, key.
    pub fn label(self, mac: bool) -> String;
    /// Every chord with this key and a subset of these modifiers (1 to 16 of them).
    pub fn with_modifier_subsets(self) -> Vec<Chord>;
}
```

- A hotkey needs Control, Option/Alt or Command/Meta, unless its key is F1–F20. Shift alone isn't enough (it would eat capital letters).
- Reserved on the Mac (a table in `keys.rs`): Command with Q, W, H, M, Tab, Space, C, V, X, A, Z or S, with or without Shift. They belong to every app.
- A chord is a **place on the keyboard**, not a letter. A chord the user sets is replayed from the same place, so it is right on every layout; only its label assumes the US layout.
- Chords that Grape itself authors by letter (Copy, Paste, Lock screen, Screenshot) are **character chords**: §4's `send_char_chord` finds the key for the letter on the current layout.
- A **Keys step** may be any chord, modifiers or none, except the wheel's own hotkey.
- The default hotkey is Control+Option+G. The feature is **off** until the user turns it on, so an update never grabs a key by itself.

## 4. Platform entry points (`grape-platform::shortcuts`)

All return `Result<_, PlatformError>` unless noted. Off macOS every one is `Unsupported` (`input_access()` is `NotNeeded`, `held_modifiers()` is `NONE`, `local_date_time()` is `None`). `PlatformError` gains no variant that carries a `String`.

```rust
pub enum HotkeyId { Open, Escape, Confirm }
pub struct FrontApp { pub pid: i32, pub id: Option<String> }   // bundle id. Debug prints the pid only.
pub struct PressContext { pub cursor: Option<(i32, i32)>, pub front: Option<FrontApp>, pub button_down: bool }
pub enum HotkeySignal { Pressed(HotkeyId, PressContext), Released(HotkeyId) }

pub trait HotkeyMonitor: Send {
    fn start(&mut self, tx: SyncSender<HotkeySignal>) -> Result<(), PlatformError>;
    /// Bind `id` to these chords (any of them fires it), replacing what it had. Empty unbinds.
    /// Returns only once the OS has done it.
    fn bind(&mut self, id: HotkeyId, chords: &[Chord]) -> Result<(), PlatformError>;
    fn stop(&mut self);
}
pub fn hotkey_monitor() -> Result<Box<dyn HotkeyMonitor>, PlatformError>;

pub enum PointerSignal { Moved { x: i32, y: i32 }, Down { x: i32, y: i32 }, Up { x: i32, y: i32 } }
pub trait PointerWatch: Send {
    fn start(&mut self, tx: SyncSender<PointerSignal>) -> Result<(), PlatformError>;
    /// While on: one `Moved` at once, then `Moved` only when the position changed, and `Down`/`Up`
    /// for primary-button changes seen while on (a button already down sends no `Down`).
    fn set_active(&mut self, on: bool);
    fn stop(&mut self);
}
pub fn pointer_watch() -> Result<Box<dyn PointerWatch>, PlatformError>;

pub enum InputAccess { Granted, Denied, NotNeeded }
pub fn input_access() -> InputAccess;                          // infallible
pub fn request_input_access(open_settings: bool) -> Result<(), PlatformError>;
pub fn held_modifiers() -> Modifiers;                          // infallible
pub struct LocalTime { pub year: i32, pub month: u8, pub day: u8, pub hour: u8, pub minute: u8 }
pub fn local_date_time() -> Option<LocalTime>;

pub fn send_chord(chord: Chord) -> Result<(), PlatformError>;
pub fn send_char_chord(ch: char, mods: Modifiers) -> Result<(), PlatformError>;
/// Types `text`; `cancel` is checked between chunks.
pub fn type_text(text: &str, cancel: &AtomicBool) -> Result<(), PlatformError>;
pub fn clipboard_text() -> Result<Option<String>, PlatformError>;
pub fn clipboard_change_count() -> Result<i64, PlatformError>;
pub fn clear_clipboard() -> Result<(), PlatformError>;

pub enum OpenTarget<'a> { App { path: &'a Path, id: Option<&'a str> }, Path(&'a Path), Url(&'a str) }
/// Waits for the opener to answer, so a target that can't be opened is an error.
pub fn open_target(target: OpenTarget<'_>) -> Result<(), PlatformError>;

pub struct Program { pub program: PathBuf, pub args: Vec<OsString>, pub stdin: Option<String> }
pub enum ProgramRequest<'a> { AppleScript(&'a str), Shell(&'a str), AppleShortcut(&'a str), KeyboardMaestro(&'a str), Speak(&'a str) }
/// The program that does this here. The app runs it.
pub fn program_for(request: ProgramRequest<'_>) -> Result<Program, PlatformError>;
/// Ends a program started in its own process group, and everything it started.
pub fn kill_group(pid: u32);

pub enum SystemControl { SleepDisplay, LockScreen, Screensaver, MissionControl, ToggleMute, VolumeUp, VolumeDown, ToggleDarkMode, ScreenshotArea }
pub enum SystemPlan { Program(Program), CharChord { ch: char, mods: Modifiers } }
pub fn system_plan(control: SystemControl) -> Result<SystemPlan, PlatformError>;

pub enum Placement { LeftHalf, RightHalf, TopHalf, BottomHalf, Fill, Center, TopLeft, TopRight, BottomLeft, BottomRight }
/// Pure: where `placement` puts a window of `size` on a display's `work_area`.
pub fn placement_frame(placement: Placement, work_area: Rect, size: (i32, i32)) -> Rect;
/// Move the focused window of `pid` (needs input access).
pub fn place_window(pid: i32, placement: Placement) -> Result<(), PlatformError>;

/// Give the front back to the app the user was in, if Grape took it. Queue it after the overlay's hide.
pub fn give_back_front();
/// Wait up to `timeout` for `pid` to be the app in front, handing the front back whenever Grape has it.
/// Call from a worker thread, never the main one.
pub fn ensure_front(pid: i32, timeout: Duration) -> bool;

pub enum PickKind { App, FileOrFolder }
pub struct Picked { pub path: PathBuf, pub folder: bool, pub app: Option<AppIdentity> }
pub struct AppIdentity { pub id: String, pub name: String }
pub fn pick_target(kind: PickKind, owner: Option<isize>) -> Result<Option<Picked>, PlatformError>;
```

### macOS notes for the implementer

- **Hot keys: Carbon** (`RegisterEventHotKey`), declared by hand in `macos/hotkey.rs` with `#[link(name = "Carbon", kind = "framework")]`. No new crate, no permission. One `InstallEventHandler` on `GetApplicationEventTarget()` for class `'keyb'` kinds 5 (pressed) and 6 (released); read the id with `GetEventParameter(event, '----', 'hkid', …)`. Carbon modifier masks: Command `0x0100`, Shift `0x0200`, Option `0x0800`, Control `0x1000`. `EventHotKeyID` and `EventTypeSpec` are two `u32` each, `#[repr(C)]`.
  - Everything Carbon runs on the main thread, and the monitor keeps its `EventHotKeyRef`s in main-thread state (the trait is `Send`; raw refs are not). `bind` hops there with a waiting `run_on_main` and returns the result; that is safe from the controller's thread because the main thread never waits for it.
  - The pressed handler reads the cursor, the pressed buttons and the front app, then `try_send`s. It takes no lock a `bind` caller could hold.
  - Carbon matches modifiers exactly, and a registration almost never fails even when another app has the chord. A real failure is `PlatformError::Os`; Settings words it "macOS wouldn't give Grape that shortcut."
  - A failed unregister is logged and tried again on the next bind: a bare Esc or Return left registered would be dead everywhere until Grape quits.
- **Escape and Confirm** are hot keys too (Escape `53`, Return `36`), bound only while a Shortcuts wheel is open. Because Carbon matches exactly and the user may still be holding the hotkey's modifiers, each is bound once for **every subset of the Open chord's modifiers** (`Chord::with_modifier_subsets`). That is how Grape reads Esc without a key monitor. While bound they are Grape's: the app in front doesn't see them. Command+Option+Esc stays the system's.
- **The front app** is read in the pressed handler, on the main thread, before anything of Grape's shows: `NSWorkspace.frontmostApplication` → pid and `bundleIdentifier` (which may be missing). Grape itself in front → `None`.
- **The pointer** comes from polling `NSEvent.mouseLocation` and `NSEvent.pressedMouseButtons` on a `"grape-pointer"` thread every 8 ms while active (the drag poller's own trick; no permission; it sees a click the overlay swallows), flipped with `mac_rules::cursor_to_grape`. The thread holds an `NSProcessInfo` activity while active, as the drag poller does. If tap-to-click proves shorter than a poll on a Mac, the fallback is a local `NSEvent` *mouse* monitor on the main thread, which needs no permission.
- **Input access** is Accessibility: `AXIsProcessTrusted` / `AXIsProcessTrustedWithOptions` with `kAXTrustedCheckOptionPrompt`, declared by hand against `ApplicationServices`. `request_input_access(false)` shows the system's own prompt (which also puts Grape in the list). With `true` it also opens `x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility` with `/usr/bin/open`; only the Settings button asks for that, and only when a press finds Grape still untrusted.
  - The DMG is ad-hoc signed (D45), and macOS ties an ad-hoc app's grant to that exact build. After an update the list still shows Grape switched on while the check says no. The copy says so: "If Grape is already in that list, remove it with − and add it again."
- **Posting keys:** `objc2-core-graphics`' `CGEvent`, with the manifest features `CGEvent`, `CGEventTypes`, `CGRemoteOperation` and `CGEventSource` (no new package; not `post_to_pid`, which needs `libc` there). Events come from a private source (`CGEventSource::new(CGEventSourceStateID::Private)`), their flags are the event's own minus Shift, Control, Option and Command, plus the chord's, and key down then key up are posted at the HID tap a few milliseconds apart. Flag masks: Shift `0x20000`, Control `0x40000`, Option `0x80000`, Command `0x100000`. An event that can't be made fails the step.
- **Character chords:** `send_char_chord` finds the virtual key that types `ch` on the current layout (`TISCopyCurrentKeyboardLayoutInputSource`, `kTISPropertyUnicodeKeyLayoutData`, `UCKeyTranslate` over keys 0–127 with no modifiers and no dead keys; declared by hand against Carbon; main thread) and falls back to `mac_rules`' US table when the layout has no data or no such key.
- **Typing:** `type_text` sends chunks of at most 20 UTF-16 units that never end inside a surrogate pair or just before a combining mark or a zero-width joiner; each chunk is a key down and key up on virtual key 0 from the private source with all four modifier flags cleared, 3 ms apart. `\n` is Return and `\t` is Tab.
- **Windows of other apps:** `AXUIElementCreateApplication(pid)`, `AXUIElementSetMessagingTimeout(app, 1 s)`, `"AXFocusedWindow"` (else `"AXMainWindow"`), read `"AXPosition"` and `"AXSize"` (`AXValueGetValue`, CGPoint type 1, CGSize type 2), `placement_frame` on the display holding the window's center, then set position, size, position. AX points have a top-left origin, the same as Grape's. A window that refuses is a step failure.
- **Programs** (`mac_rules`, each a pure builder with tests). Scripts go in on stdin, so they never show in the process list:
  - AppleScript: `/usr/bin/osascript -` with the source on stdin.
  - Shell: `/bin/zsh -l -s` with the source on stdin (a login shell, so the user's own `PATH`).
  - Apple Shortcut: `/usr/bin/shortcuts run <name>`.
  - Keyboard Maestro: `/usr/bin/osascript -e 'on run argv' -e 'tell application "Keyboard Maestro Engine" to do script (item 1 of argv)' -e 'end run' -- <name>`. The name is an argument, never spliced into the script.
  - Speak: `/usr/bin/say <text>`; a text starting with `-` gets a leading space.
  - Open: `/usr/bin/open -b <id>`, falling back to `-a <path>`; `/usr/bin/open <path>`; `/usr/bin/open <url>`.
- **System controls:** Sleep display `/usr/bin/pmset displaysleepnow`; Screensaver `open -a ScreenSaverEngine`; Mission Control `open -a "Mission Control"`; mute and volume through one-line `osascript` (`set volume …`, no prompt); dark mode through `System Events` (one Automation prompt); Lock screen is the character chord Control+Command+`q`; Screenshot area is Command+Shift+`4`.
- **`give_back_front`** is one closure on the main thread that calls the existing `hand_back_activation` (a no-op unless Grape is active with no key window). The app queues it with `run_on_main_thread`, the same queue as the overlay's hide, so it runs after it.
- **`ensure_front`** loops for the whole timeout: every 10 ms, hand back if Grape is active, and finish when `frontmostApplication` is `pid`.
- **Pickers:** `NSOpenPanel`, as `pick_folder` does, on a blocking worker like Settings' folder picker. Apps start in `/Applications` and allow only `.app` (`setAllowedFileTypes` under `#[allow(deprecated)]`: the newer call would add a package). The identity is `NSBundle`'s `bundleIdentifier` (feature `NSBundle`) and `NSFileManager`'s display name.
- **The clock:** `local_date_time` uses `libc`'s `localtime_r`, already linked.
- `Info.plist` gains `NSAppleEventsUsageDescription` (AppleScript steps that talk to other apps), in Grape's words. When M9 turns the hardened runtime on, it needs the `com.apple.security.automation.apple-events` entitlement too.
- Unit tests cover `mac_rules` and the pure parts only. Nothing in a test registers a hot key, posts a key, opens a panel or calls `run_on_main`.

## 5. The wheel session (`shortcuts/session.rs`, driven by `controller.rs`)

New controller inputs: `Input::Hotkey(HotkeySignal)`, `Input::Pointer(PointerSignal)`, `Input::Shortcuts(Arc<Snapshot>)` (the enabled flag, where it opens and the menus; sent at start and after every change with a waiting `send`, never the dropping `try_send`), `Input::RunEnded` (the runner is free again).

New effects, emitted **only** from `State::Shortcuts`; a drag wheel's effect sequences don't change:
- `Effect::Track(bool)`: start or stop the pointer watch, and bind or unbind Escape and Confirm.
- `Effect::GiveBack`: queue `give_back_front` after the overlay's hide.
- `Effect::RunShortcut(ShortcutRun)`.

`Open`, `ShowOverlay`, `Highlight`, `Close` and `HideOverlay` are reused, so the haptic comes for free; the tick needs §6's page change.

New state: `State::Shortcuts(…)`, holding `seq` (from the controller's own counter, shared with the drag wheels; the session never numbers a wheel), the display, the anchor, the front app, the trail of open groups, the current ring's items, the highlight, the last pointer position, `sticky`, whether a button-down was seen, whether it was painted, the press time and the last event time. The anchor math (logical offsets, the hit test, the global anchor) is one helper both wheel states use.

| In state | Input | Result |
|---|---|---|
| Idle, on, not paused, no run in flight | `Pressed(Open, ctx)` with a cursor, a display and no button down | choose the menu for `ctx.front`; anchor = the cursor (or the work area's center), clamped; `Open`, `ShowOverlay`, `Track(true)`; → Shortcuts, not sticky |
| Idle | `Pressed(Open, …)` while off or paused, with a button down, or with no cursor or display | nothing (a log line) |
| Idle | `Pressed(Open, …)` while a run is in flight | nothing; the app shows "Still running" |
| Idle | `Released(*)`, `Pressed(Escape or Confirm)`, any pointer signal | ignored, no log (they were in flight when the wheel closed) |
| Dragging, Open, Dismissed | any hotkey or pointer input | ignored: a drag always wins |
| Shortcuts | `Pointer::Moved` | remember the position; hit test → `Highlight` on change |
| Shortcuts | `Pointer::Down` | remember that a press was seen |
| Shortcuts | `Pointer::Up` after a seen press | on a segment → **pick** it; nearer the anchor than `INNER_R` → back one group, or cancel at the root; farther than `OUTER_R` → cancel; in a gap or a cut-off corner → nothing |
| Shortcuts, not sticky | `Released(Open)` | under `TAP` (300 ms) since the press → sticky, whatever is highlighted; otherwise highlight → **pick** it; otherwise cancel |
| Shortcuts, sticky | `Released(Open)` | ignored |
| Shortcuts, not sticky | `Pressed(Open, …)` | ignored (a repeat) |
| Shortcuts, sticky | `Pressed(Open, …)` | cancel |
| Shortcuts | `Pressed(Confirm, …)` | highlight → **pick** it; otherwise nothing |
| Shortcuts | `Pressed(Escape, …)` | close, reason `escape` |
| Shortcuts | `Released(Escape or Confirm)` | ignored |
| Shortcuts | `Painted { seq }` for this wheel | log the milliseconds since the press, once |
| Shortcuts | no `Painted` 1 s after the open | close, reason `watchdog` (an undrawn wheel must not hold Esc, Return and the screen) |
| Shortcuts | `DragSignal::Started` | cancel, then handle the drag as from Idle |
| Shortcuts | `DragSignal::Moved`, `Modifiers`, `Escape` | ignored |
| Shortcuts | `DragSignal::Ended` | cancel (a `Started` was missed) |
| Shortcuts | `DropEvent::Enter` | cancel (some drag reached the wheel); other drop events are ignored without a log line |
| Shortcuts | `SetPaused(true)` | close, reason `paused`; → Idle |
| Shortcuts | `Configure`, `Shortcuts(…)` | stored; an open wheel isn't redrawn |
| Shortcuts | 30 s with no input | close, reason `watchdog` |

- **One teardown.** `WheelController::close(reason, next)` handles both wheels. For `State::Shortcuts` it emits `Close`, `HideOverlay`, `Track(false)`, `GiveBack`, in that order. No code path may replace a `Shortcuts` state except through it; the wildcard arm of `DragSignal::Ended` becomes explicit per state. `next_deadline()` and `tick()` cover `State::Shortcuts` (the paint deadline, then `last_event + WATCHDOG`).
- **Pick, a shortcut with steps:** the teardown with reason `picked`, then `RunShortcut`. → Idle. A shortcut with nothing to run (no steps, or only unset ones) still closes; the app shows "Nothing to run yet".
- **Pick or leave a group:** change the trail, clear the highlight, send `Open` with the same `seq` (an update: no animation), then hit-test the last pointer position and send `Highlight` if it is on a segment. Become sticky.
- **Cancel / close:** the teardown. → Idle.
- **An empty menu** opens with no segments and shows the "nothing here yet" message. It closes like any other.
- `last_event` is refreshed by hotkey signals and pointer signals only, not by `Painted`, `Configure`, `Shortcuts` or anything ignored.
- The Open hot key is **bound only while the feature is on and Grape isn't paused**, so a paused or switched-off Grape never swallows the chord. The "off or paused" row is a guard for a press already in flight.
- Hit testing is `grape_core::hit_test` on logical offsets from the anchor, exactly as for drags.

`ShortcutRun { seq, name, steps, front: Option<FrontApp>, anchor, display }`. `ShortcutRun`, `Snapshot`, `FrontApp` and every model type that holds something the user typed implement `Debug` **by hand**, printing kinds and counts only (`Input` and `Effect` derive `Debug`, and a derived one would put a script in the log the first time someone printed an effect). A test checks a sample holding a marker string never prints it. The pick's log line says "segment {index}", never the name.

## 6. The overlay contract

`wheel://open` keeps its keys. `content` gains one kind, and `mode` is `"shortcuts"` exactly when the content kind is:

```json
{"kind":"shortcuts","dense":false,"trail":["Edit"],"segments":[
  {"index":0,"center":1.5707963267948966,"start":3.141592653589793,"end":0.0,"labelX":0.0,"labelY":-84.0,
   "title":"Save","icon":{"kind":"emoji","value":"💾"},"group":false}]}
```

- `icon` is `{"kind":"none"}`, `{"kind":"emoji","value":"…"}` or `{"kind":"glyph","value":"<id>"}`. Glyph ids come from one list shared by Rust and the UI (`GLYPHS`): Grape's own 24×24 stroke drawings. An id the page doesn't know draws no icon.
- `segments` may be empty: the page then shows the empty message. `trail` is the open groups' names, outermost first.
- `title` is the shortcut's name as typed. Guards check user-typed strings (`title`, `trail`, an emoji's `value`) for their type only, never their length: a guard that disagreed with Rust about length would drop the open while Rust held Esc.
- `wheel://close` gains the reason `"picked"`.
- New fixtures: `wheel-open-shortcuts.json`, `wheel-open-shortcuts-empty.json`, `wheel-close-picked.json`.

Rust (`events.rs`): `pub enum UiMode { Conversions, Tools, Actions, Shortcuts }` (camelCase) with `From<WheelMode>` and `PartialEq<WheelMode>`, used only as `WheelOpen.mode`; `OpenWheel.mode`, `DropReport.mode`, `KeyMap::mode_for` and the panel keep `grape_core::WheelMode`. `UiContent::Shortcuts { dense, trail, segments: Vec<UiShortcutSegment> }` with `UiShortcutSegment { #[serde(flatten)] geo: Segment, title, icon, group }`. `CloseReason::Picked`. The three existing `wheel-open-*` fixtures must still serialize byte for byte.

The page (forced edits):
- `shared/events.ts`: `WheelMode` and `WHEEL_MODES`, `CloseReason` and `CLOSE_REASONS`, a `WheelIcon` union, a `ShortcutSegment` (keys `index, center, start, end, labelX, labelY, title, icon, group`; no `entry`), a `"shortcuts"` case in `isWheelContent` that allows zero segments, and the mode/kind pairing.
- `shared/events.test.ts`: the three fixtures in `CASES`, `"picked"` in the reason list, and `shortcuts-view-*.json` and `shortcuts-step-*.json` excluded from the glob the way `settings-view-*` is.
- `wheel/scene.ts`: `validHighlight` accepts the new kind (today it answers `null` for anything but `entries`, so nothing would highlight or tick), `buildScene` and `WheelScene` gain the kind, and the center shows the hovered title, else the last `trail` name.
- `wheel/copy.ts`: `MODE_NAMES`, `wheelLabel`, and the empty message, in Grape's words: "No shortcuts here yet. Add some in Settings."
- Labels: a new `shortcutLabelLines(title, dense)` that breaks a word longer than the line at a code-point boundary and ends a cut line with "…" (`wrapLines` and its tests stay as they are). A segment with an icon shows the icon over one line of label; at `dense` (7 or more) it shows the icon alone, and the title shows in the center on hover and in the segment's `aria-label` and `<title>`. Group segments carry a small mark.
- The page still decides nothing and reads no pointer: Rust does. But a sticky wheel is the first time the overlay's page gets real pointer events, so it cancels `contextmenu`, `mousedown` and `dragstart` (no WebKit menu, selection or page drag).
- Tests: a highlight on a shortcuts payload paints and ticks; the labels for a 24-unit unbroken name, a CJK name and an emoji name.

## 7. The model and `shortcuts.json`

**Limits** (every length is UTF-16 code units, counted in Rust with `encode_utf16().count()`, so the page's `maxlength` agrees): 12 shortcuts in a ring (`MAX_ITEMS`); groups hold shortcuts with steps only (two levels); 16 steps (`MAX_STEPS`); names 1–24 after trimming, with no control characters or line breaks; an emoji icon's text 1–16 with no whitespace or controls; text fields 4 000; scripts 20 000; URLs 2 048; a delay 10 ms–60 s; 40 menus; the file 1 MiB.

```json
{"version":1,"enabled":false,"hotkey":{"key":"KeyG","shift":false,"ctrl":true,"alt":true,"meta":false},
 "opensAt":"cursor",
 "menus":[{"scope":{"kind":"everywhere"},"withEverywhere":false,"items":[
   {"name":"Copy","icon":{"kind":"glyph","value":"clipboard"},"kind":"steps","steps":[
     {"kind":"clipboard","action":"copy"}]}]}]}
```

Ids are **not** in the file. Rust numbers menus, shortcuts and steps from 1 when it loads and keeps the counter in memory; ids are unique across the whole file and appear only in the view.

Step kinds (stored form): `keys {chord|null}`, `typeText {text}`, `clipboard {action: copy|paste|clear}`, `openApp {path|null, bundleId|null, name|null}`, `openFile {path|null, folder}`, `openUrl {url}`, `appleShortcut {name}`, `appleScript {source}`, `shell {source}`, `keyboardMaestro {name}`, `window {placement}`, `system {control}`, `notify {text}`, `speak {text}`, `delay {ms}`. An app menu's scope is `{"kind":"app","bundleId":"…","name":"…"}`.

- **Menu choice:** the menu whose scope is the front app's id, else Everywhere (also when the front app has no id). An app menu with `withEverywhere` on ends with one extra group, "Everywhere", holding the Everywhere menu's shortcuts (its groups left out: two levels only); it is left out when that would pass 12, and the view's `note` says so when the switch is turned on.
- **Text variables** exist in `typeText` only: `{date}` (2026-11-23), `{time}` (14:05), `{clipboard}`. Unknown braces stay as typed, and so do `{date}` and `{time}` when the clock is unknown. Expansion is one pass (a clipboard holding `{date}` isn't expanded again) and a pure function of the text and the three values.
  - The clipboard's value has every control character removed, with `\r`, `\n` and `\t` each turned into one space, and is cut so the whole expanded text stays within 4 000: copied text typed into a terminal must not run as commands. No text on the clipboard expands to nothing. The pasteboard is read only when the text holds `{clipboard}`.
  - Stored `typeText` itself allows `\n` and `\t` only; `\r\n` is stored as `\n`; any other control character is refused.
- **URLs** match `^[a-z][a-z0-9+.-]{0,31}:` followed by at least one character, with nothing at or below U+0020 and nothing in U+007F–U+009F. `file` is refused because the page never sends a path (D19); `javascript` and `data` because they open nothing useful. Any other scheme opens whatever app registered it: the user typed it, and the rule is a syntax check, not a safety boundary.
- **Names** for Apple Shortcuts and Keyboard Maestro macros: no control characters, and not starting with `-`.
- A Keys step with no chord yet, an open step with no target yet and a shortcut with no steps are valid stored states. Unset steps are skipped at run time.
- **Starter set:** the in-memory default for a missing file, on the Mac: Everywhere holds Copy, Paste, Screenshot, Mission Control, Mute and Lock screen (clipboard and system steps only, so they are right on every keyboard layout). Nothing is written until the first change.
- While the feature isn't available on this OS, nothing is loaded, created or written, and every changing command is refused with one sentence.
- **Loading is total**, in two passes. First `Top { version, enabled, hotkey, opensAt, menus: Vec<Value> }` with the scalars as raw values: a file that isn't an object, has no `version` or no `menus` array, has a newer `version`, or is over 2 MiB is moved aside as `shortcuts.unreadable.json` (the settings file's rule, with its own stem) and the defaults are used. `enabled`, `hotkey` and `opensAt` each fall back alone (a hotkey that names an unknown key or fails `hotkey_problem` becomes none, and `enabled` false). Then each menu and each shortcut is read by itself, with unknown fields refused. A bad step drops its **whole shortcut** (running the rest of it would do something else); a bad shortcut drops alone; a second Everywhere menu, a second menu for one app, a group inside a group and anything past the limits are dropped; a missing Everywhere menu is added empty.
- When a load dropped anything, the file is copied to `shortcuts.unreadable.json` (the next free name) before the first save, and the view's `note` says shortcuts were left out.
- **Nothing a user typed is ever logged:** not names, text, scripts, URLs or paths, and never a serde message (it quotes values). The log counts drops by kind and gives the error's class, line and column, as `settings.rs`' `parse_problem` does.
- **Saving:** the path is `app.path().resolve("shortcuts.json", BaseDirectory::AppData)`; no store is opened on it. Create the folder, write `shortcuts.json.tmp` (mode 0600 on Unix), `sync_all`, then rename it over the file (retrying a few times on a Windows `PermissionDenied`). After every change. A failed save still applies the change and says "Couldn't save your shortcuts." A change that would pass 1 MiB is refused.
- The file is its own, not a key in `settings.json`: a broken one never takes the other with it.
- **Plain text:** the file, its `.tmp` and any moved-aside copy hold typed text and scripts in the clear, and the file is as trusted as the user's own account: anything that can write it can make Grape act. D47 records both as accepted, and the Settings section says "Shortcuts are saved as plain text on this Mac. Don't put passwords in them."

## 8. Settings commands and view

All granted to the Settings window only (each in `build.rs`' list, `generate_handler!` and `capabilities/settings.json`), each answering `Result<ShortcutsView, String>`. The page never sends a path: apps, files and folders are picked on the Rust side. It does send what the user typed (names, text, scripts, URLs); D47 says so. **No command runs a shortcut.**

| Command | Args | Notes |
|---|---|---|
| `shortcuts_get` | `{}` | |
| `shortcuts_set_enabled` | `{on}` | Refused while there is no hotkey. Waits for the bind. |
| `shortcuts_set_hotkey` | `{chord\|null}` | Waits for the bind; a failure leaves the old chord bound and is refused with "Grape couldn't use that shortcut. Try another." `null` also turns the feature off. |
| `shortcuts_set_opens_at` | `{at: "cursor"\|"center"}` | |
| `shortcuts_request_access` | `{}` | The system prompt; the settings pane when a later press finds Grape still untrusted. |
| `shortcuts_add_menu` | `{}` | Opens the app picker. An app that has a menu already, or Grape itself, is refused. |
| `shortcuts_remove_menu` | `{menu}` | Everywhere can't be removed. |
| `shortcuts_set_menu_everywhere` | `{menu, on}` | |
| `shortcuts_add_item` | `{menu, group\|null, kind: "steps"\|"group"}` | Rust names it "New shortcut" or "New group". |
| `shortcuts_set_item` | `{item, name, icon}` | |
| `shortcuts_remove_item` | `{item}` | |
| `shortcuts_move_item` | `{item, to}` | `to` is a 0-based place in the item's own ring or group; out of range is refused. No moves into or out of a group in this version. |
| `shortcuts_add_step` | `{item, kind}` | |
| `shortcuts_set_step` | `{step, value}` | `value` is a `StepEdit`. |
| `shortcuts_remove_step` | `{step}` | |
| `shortcuts_move_step` | `{step, to}` | Inside its own shortcut. |
| `shortcuts_pick_target` | `{step}` | An `openApp` step gets the app picker; an `openFile` step one panel for files and folders (`folder` comes from what was picked). |

One picker at a time, shared by `add_menu` and `pick_target`.

```rust
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
enum StepEdit {
    Keys { chord: Option<ChordWire> }, TypeText { text: String }, Clipboard { action: ClipboardAction },
    OpenUrl { url: String }, AppleShortcut { name: String }, AppleScript { source: String }, Shell { source: String },
    KeyboardMaestro { name: String }, Window { placement: PlacementId }, System { control: ControlId },
    Notify { text: String }, Speak { text: String }, Delay { ms: u32 },
}
struct ChordWire { key: String, shift: bool, ctrl: bool, alt: bool, meta: bool }   // deny_unknown_fields; key through KeyCode::from_id
```

There is no `OpenApp` or `OpenFile` edit: targets change only through `shortcuts_pick_target`. A `kind` that differs from the stored step's is refused with one fixed sentence.

```json
{"available":true,"enabled":false,
 "hotkey":{"chord":{"key":"KeyG","shift":false,"ctrl":true,"alt":true,"meta":false},"label":"Control+Option+G"},
 "binding":"off","access":"denied","opensAt":"cursor","note":null,
 "limits":{"items":12,"steps":16,"name":24,"icon":16,"text":4000,"script":20000,"url":2048,"menus":40,"delayMinMs":10,"delayMaxMs":60000},
 "keys":[{"id":"KeyA","label":"A"}],
 "modifiers":{"shift":"Shift","ctrl":"Control","alt":"Option","meta":"Command"},
 "glyphs":["key","text"],
 "menus":[{"id":1,"scope":{"kind":"everywhere"},"withEverywhere":false,"items":[
   {"id":2,"name":"Copy","icon":{"kind":"glyph","value":"clipboard"},"kind":"steps","steps":[
     {"id":3,"kind":"clipboard","action":"copy"}]}]}]}
```

- `hotkey` is `null` when none is set. `binding` is `"off"` (switched off, no hotkey, or not available), `"bound"` or `"failed"`. `access` is `"granted"`, `"denied"` or `"notNeeded"`.
- `note` is a sentence about the change that made this view (shortcuts left out at load, "Everywhere didn't fit in this menu"), `null` otherwise.
- An app menu's scope is `{"kind":"app","name":"Safari"}`: the name only. `openApp` and `openFile` steps carry `name` (the display name, or `null` until picked) and never a path. A Keys step carries `chord` and its `label`.
- **Entering a chord** is four labelled checkboxes and one `<select>` of keys, filled from `keys` and `modifiers`. Not a keydown recorder: the webview can't see a chord Carbon already took, and Command+W would close the window.
- **The page** mounts with `mountShortcuts(root, { invoke })` into its own `<div id="shortcuts-root">` after `#settings-root`, with its own command queue, status line and `shortcuts_get`. `mountSettings` and its tests don't change. Inside the root: one `<section>` with an `h2`; menus and shortcuts as `div`s and `h3`s; every control with an id built from Rust's ids (`sc-item-7-name`, `sc-step-9-text`) and exactly one label; every button with an `aria-label` that contains its text.
- **Text fields** send on `change` (blur, or Enter in a one-line field), never on `input`. A field whose value differs from the last view isn't overwritten by a redraw until its own command has answered, and every such field is sent on `pagehide` and when the page is hidden. Rows are keyed by Rust's id and never rebuilt while they hold focus; after a move the page focuses the moved row's button itself.
- The page reads `shortcuts_get` again when its window gets the focus or becomes visible, which is how it learns that access was granted in System Settings.
- On Windows for now `available` is `false`, and the section says the wheel is on the Mac first.
- Fixtures: `shortcuts-view-macos.json`, `shortcuts-view-windows.json`, and one `shortcuts-step-*.json` per `StepEdit` shape, each read by both sides' tests.

## 9. Running a shortcut (`shortcuts/run.rs`)

- One run at a time, on a `"grape-shortcut"` thread. While one runs, the Open hot key shows "Still running {name}" and opens no wheel.
- Steps run in order. The first failure stops the run and shows an error toast near the wheel's anchor: "Couldn't finish {name}" with one plain sentence.
- Before the first step that presses keys, types, copies, pastes or moves a window:
  - check `input_access()`. Denied → stop, call `request_input_access(false)` once per launch, and say "Grape needs Accessibility access to press keys. Turn it on in System Settings, then try again." (the Settings section adds the remove-and-add sentence);
  - wait up to 500 ms for the hotkey's own modifiers to be let go (`held_modifiers`); still held → stop with "Let go of the keys first.";
  - `ensure_front(front pid, 400 ms)`; false → stop: "{app} didn't come back to the front." With no front app (Grape's own window was in front) these steps stop with "Click the app you want first."
- `clipboard copy` is the character chord Command+`c`, then a wait of up to 500 ms for the pasteboard's change count to move; `paste` is Command+`v`; `clear` empties the pasteboard. `notify` is Grape's own toast. `delay` sleeps in 20 ms slices so a cancel is quick.
- Programs get the source on a stdin pipe that Grape writes and closes, or `/dev/null` when there is none; stdout and stderr go to `/dev/null`; they run in the home folder, in their own process group, for at most 60 s. A non-zero exit is a failure. Output is never read or logged. A run killed while macOS is asking the user to allow automation works on the next try.
- **Pause and Quit:** `ShortcutsState` holds the run's cancel flag and the child's pid. `shortcuts::cancel_run` is called from the tray's Quit, from Pause, and from both exit arms in `lib.rs`; it sets the flag and kills the group itself. The runner checks the flag before every step and before every hop to the main thread, and never hops after it is set. `RunEvent::Exit` waits for the run at most as long as it waits for jobs. `wheel::stop` also stops the hotkey monitor and the pointer watch. Pause stops a running shortcut, though it doesn't stop a conversion.
- A finished run shows nothing unless a step says so. The toast window is shared: a conversion's progress can replace a shortcut's toast.
- The log says which step *kinds* ran and how long, never their contents.

## 10. Order of work

1. `grape-platform`: `keys.rs`, `shortcuts.rs`, stubs, mock scripts, `mac_rules` tables (compiles on every OS, tests on Linux).
2. In parallel: the macOS implementation; the app's `model.rs` and `store.rs`; the event types and fixtures.
3. The session, the controller glue, the runner, the commands (`build.rs`, `generate_handler!`, `capabilities/settings.json`) and the wiring in `lib.rs`, `tray.rs` and `wheel.rs`.
4. The overlay drawing and the Settings section.
5. Docs: D47 (which amends D7 and the "no permission prompts" rule in `crates/grape-platform/CLAUDE.md` rather than leaving them contradicted), `INTERACTION_SPEC.md` §1, §8, §9 and a new §12, `ARCHITECTURE.md` (the "no pointer events" line, threads, settings, fixtures), `PLATFORM_MACOS.md`, `TESTING.md`, `ROADMAP.md` M10, `PRODUCT.md`, `README.md`, `m1-drag-wheel.md` §C, and the CLAUDE.md files.

Checked from Linux: `cargo test` for the pure parts, the Mac and Windows clippy cross-checks in `START_HERE.md`, the UI's typecheck and tests. CI's macOS jobs compile and link the Carbon and AX declarations. Everything a person has to see gets a `mac:` line in `TESTING.md`.
