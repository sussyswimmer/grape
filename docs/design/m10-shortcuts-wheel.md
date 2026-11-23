# M10 design: the Shortcuts wheel

Status: built (`DECISIONS.md` D47): macOS first, with stubs on Windows and the real thing there later. This note was the design, revised after three design reviews (macOS feasibility, controller, contract), and now says what the code does: where the code rightly went another way, the text below was changed to match it. A code review on 2026-11-23 changed the code again, and this note with it (D47's review bullet lists what). It ran on Linux only: `cargo test` for the pure parts, clippy with the Mac and Windows cross-checks, and the UI's typecheck and tests. Nothing here has run on a Mac or the PC (`TESTING.md` → "Shortcuts wheel, M10").

This note is the contract the code was written against, and the one a change to it is checked against, the way `m1-drag-wheel.md` is for the drag wheels. Behavior lives in `INTERACTION_SPEC.md` §12; what was learned from the reference app lives in `RADIAL_FINDINGS.md`. Where this note and those disagree, they win and this note gets fixed.

## 1. What it is

A fourth wheel that needs no drag. The user presses a global hotkey, the wheel opens at the cursor, and each segment is a **shortcut** the user made in Settings. A shortcut is a name, an icon and one or more **steps** (press a key combination, type text, open something, run a script, move a window, …), or it is a **group** that opens a second ring of shortcuts. Which **menu** shows depends on the app in front: an app's own menu if it has one, otherwise the **Everywhere** menu.

Words: *Shortcuts wheel*, *shortcut*, *step*, *group*, *menu*, *Everywhere*. These are Grape's words. Don't borrow the reference app's names for things.

Out of scope for this change (listed in ROADMAP M10 as unchecked items): AI steps and an online preset library (both need the network: golden rule 3), opening the wheel from a hot corner, a shake, a mouse button or a bare modifier, an app switcher, visual effects, appearance options, preset files, media keys, a keydown recorder for chords, a check against the system's own shortcuts, and everything on Windows.

## 2. Where the code goes

| Piece | Where | Notes |
|---|---|---|
| Keys and chords (pure) | `crates/grape-platform/src/keys.rs` | `KeyCode`, `Chord`, the reserved lists, `chords_posted_on_mac`. No serde (the crate has none). |
| Portable entry points | `crates/grape-platform/src/shortcuts.rs` | Same shape as `file_actions.rs`: `pub fn` → `imp::`. Also the pure `placement_frame`. |
| macOS | `crates/grape-platform/src/macos/{hotkey.rs,input.rs,shortcuts.rs}` | Carbon hot keys, CGEvent posting, AX window moves, pickers. |
| macOS pure rules | `crates/grape-platform/src/mac_rules.rs` | Virtual key table, Carbon and CGEvent masks, program builders, text chunks, the press rule and the hop claim. Only `macos/*` may call it. |
| Windows, other | `windows/mod.rs`, `unsupported.rs` | Every new `imp::` function returns `PlatformError::Unsupported`. |
| Scripted twins | `crates/grape-platform/src/mock.rs` | `ScriptedHotkeys` (records every `bind`, can refuse a chord) and `ScriptedPointer`, with ready scripts in `mock::scripts`, for the controller's tests. |
| Event types | `apps/desktop/src-tauri/src/events.rs` | `UiMode`, `UiContent::Shortcuts`, `UiIcon`, `GLYPHS`, `CloseReason::Picked`. |
| Model (pure) | `apps/desktop/src-tauri/src/shortcuts/model.rs` | Menus, shortcuts, steps, limits, validation, menu choice, text variables. |
| File | `apps/desktop/src-tauri/src/shortcuts/store.rs` | `shortcuts.json`: load (total), save (atomic), move aside. A `Store` carries what a load decided into the saves that follow. |
| Wheel session (pure) | `apps/desktop/src-tauri/src/shortcuts/session.rs` | The state in §5. It answers each input with a plain `Decision`, and `controller.rs` turns that into effects. Also `WheelPlace`, the anchor math both kinds of wheel use. |
| Runner | `apps/desktop/src-tauri/src/shortcuts/run.rs` | Runs steps in order behind an `Actuator` trait. |
| Commands, state, wiring | `apps/desktop/src-tauri/src/shortcuts/mod.rs` | `ShortcutsState`, the `shortcuts_*` commands, the hotkey binding. |
| Overlay | `apps/desktop/ui/src/wheel/*` (the drawings in `wheel/glyphs.ts`), `shared/events.ts` | A new content kind. Rust still owns the hit test. |
| Settings | `apps/desktop/ui/src/settings/shortcuts-{model,view}.ts` | Its own root in `settings.html`, mounted by `src/settings.ts`. |

Rules of the layout:
- `grape-core` doesn't change. `hit_test` and `wheel_layout` take only a count; the session reads `GEOMETRY.inner_r` and `outer_r` too.
- Nothing reachable from `keys.rs`, from `shortcuts.rs`' pure functions or from the app may reference `mac_rules`: that module only exists on macOS and under `cfg(test)`.
- No new package enters `Cargo.lock`. Carbon, ApplicationServices and the keyboard-layout calls are declared by hand (`UCKeyTranslate` and `LMGetKbdType` in a block that also links CoreServices, so they resolve from whichever framework exports them); `objc2-core-graphics` and `objc2-foundation` only gain features.
- The app crate has no `thiserror`, so `SaveError` and the runner's `Failure` write `Display` by hand.

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
    pub fn label(self) -> &'static str;           // "A", "4", "F5", "↑", "Space", "Return"… (the US Mac layout's names, each unique)
}

/// A key with the modifiers held with it.
pub struct Chord { pub key: KeyCode, pub mods: Modifiers }

pub enum HotkeyProblem { NeedsModifier, Reserved }
impl Chord {
    /// Why this chord can't open the wheel, if it can't.
    pub fn hotkey_problem(self, mac: bool) -> Option<HotkeyProblem>;
    /// "Control+Option+G" on the Mac, "Ctrl+Alt+G" elsewhere (Meta is "Command" or "Win"). Order: ctrl, alt, shift, meta, key.
    pub fn label(self, mac: bool) -> String;
    /// Every chord with this key and a subset of these modifiers (1 to 16 of them), the bare key first.
    pub fn with_modifier_subsets(self) -> Vec<Chord>;
}

/// The chords Grape's own steps press on the Mac, by their place on the US layout:
/// Command+C, Command+V, Control+Command+Q and Command+Shift+4.
pub fn chords_posted_on_mac() -> &'static [Chord];
```

- A hotkey needs Control, Option/Alt or Command/Meta, unless its key is F1–F20. Shift alone isn't enough (it would eat capital letters).
- Reserved on the Mac (three tables in `keys.rs`), each answered `HotkeyProblem::Reserved`, which Settings words "Every app already uses that one. Try another.":
  - Command with Q, W, H, M, Tab, Space, C, V, X, A, Z or S, with or without Shift and with neither Control nor Option. They belong to every app.
  - Control+Command+Q (Lock screen) and Command+Shift+3, 4 and 5 (the screenshots), each with exactly those modifiers, since the OS matches a hot key's modifiers exactly. They are the system's.
  - Every chord in `chords_posted_on_mac()`. The Open hot key stays bound while a shortcut runs, so a step that pressed it would open the wheel (or show "Still running") and the system would never see the chord. All four are in the two lists above; a test in `mac_rules` ties `system_plan`'s character chords, with Copy and Paste, to this list, and one in the model ties `set_hotkey` and the load to it.
  - Nothing is reserved off the Mac.
- **A limit of that list:** it is by place on the US layout, but those steps go out by character (below). On AZERTY `q` sits where the US layout has A, so Control+Command+A is still accepted there and would collide with Lock screen. Not closed. Whether a key Grape posts really fires Grape's own Carbon hot key was never seen on a Mac either; the reservation is harmless if it doesn't.
- `KeyCode` has 81 keys. Its labels are the US Mac names: Enter is "Return", Escape "Esc", Backspace "Delete", Delete "Forward Delete", and punctuation is the sign itself.
- A chord is a **place on the keyboard**, not a letter. A chord the user sets is replayed from the same place, so it is right on every layout; only its label assumes the US layout.
- Chords that Grape itself authors by letter (Copy, Paste, Lock screen, Screenshot) are **character chords**: §4's `send_char_chord` finds the key for the letter on the current layout.
- A **Keys step** may be any chord, modifiers or none, except the wheel's own hotkey. The rule holds in both directions: `set_step` refuses a Keys chord equal to the hotkey ("That one opens the wheel. Pick another."), `set_hotkey` refuses a chord a Keys step already presses ("One of your shortcuts presses that. Try another."), and a load treats a Keys step equal to the file's hotkey as a bad step (§7). Otherwise a shortcut could press the key that opens the wheel.
- The default hotkey is Control+Option+G. The feature is **off** until the user turns it on, so an update never grabs a key by itself.

## 4. Platform entry points (`grape-platform::shortcuts`)

All return `Result<_, PlatformError>` unless noted. Off macOS every one is `Unsupported` (`input_access()` is `NotNeeded`, `held_modifiers()` is `NONE`, `local_date_time()` is `None`, `ensure_front` is `false`, and `kill_group` and `give_back_front` do nothing). `PlatformError` gains no variant that carries a `String`; `AlreadyRunning` now prints "already running", since a second hot key monitor gets it too, not only a drag monitor. Every type here that holds something typed or picked (`FrontApp`, `OpenTarget`, `Program`, `ProgramRequest`, `Picked`, `AppIdentity`) writes `Debug` by hand, printing kinds and counts only.

```rust
pub enum HotkeyId { Open, Escape, Confirm }
pub struct FrontApp { pub pid: i32, pub id: Option<String> }   // bundle id. Debug prints the pid only.
pub struct PressContext { pub cursor: Option<(i32, i32)>, pub front: Option<FrontApp>, pub button_down: bool }  // any mouse button
pub enum HotkeySignal { Pressed(HotkeyId, PressContext), Released(HotkeyId) }

pub trait HotkeyMonitor: Send {
    fn start(&mut self, tx: SyncSender<HotkeySignal>) -> Result<(), PlatformError>;
    /// Bind `id` to these chords (any of them fires it), replacing what it had. Empty unbinds.
    /// Returns only once the OS has done it. On an error `id` keeps what it had.
    fn bind(&mut self, id: HotkeyId, chords: &[Chord]) -> Result<(), PlatformError>;
    /// Stops listening and lets go of every chord.
    fn stop(&mut self);
}
pub fn hotkey_monitor() -> Result<Box<dyn HotkeyMonitor>, PlatformError>;

pub enum PointerSignal { Moved { x: i32, y: i32 }, Down { x: i32, y: i32 }, Up { x: i32, y: i32 } }
pub trait PointerWatch: Send {
    fn start(&mut self, tx: SyncSender<PointerSignal>) -> Result<(), PlatformError>;
    /// While on: one `Moved` at once, then `Moved` only when the position changed, and `Down`/`Up`
    /// for primary-button changes seen while on (a button already down sends no `Down`, but its
    /// release does send `Up`).
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
/// Types `text`; `cancel` is checked between chunks, and a cancel ends it early without an error.
pub fn type_text(text: &str, cancel: &AtomicBool) -> Result<(), PlatformError>;
pub fn clipboard_text() -> Result<Option<String>, PlatformError>;
pub fn clipboard_change_count() -> Result<i64, PlatformError>;
pub fn clear_clipboard() -> Result<(), PlatformError>;

pub enum OpenTarget<'a> { App { path: &'a Path, id: Option<&'a str> }, Path(&'a Path), Url(&'a str) }
/// Waits for the opener to answer, with no limit and no way to cancel. Not for a step: a run uses `open_programs`.
pub fn open_target(target: OpenTarget<'_>) -> Result<(), PlatformError>;
/// The programs that open `target`, to try in order: the first that exits with 0 has opened it.
/// Never empty on macOS. Nothing runs here: the app runs them like any `Program`.
pub fn open_programs(target: OpenTarget<'_>) -> Result<Vec<Program>, PlatformError>;

/// `stdin`: written to the program's standard input, then closed; `None` is `/dev/null`.
/// `script`: a script the program runs from a file. Whoever runs the program writes it to a new
/// file (mode 0600) in a new folder of its own (mode 0700), appends the file's absolute path as
/// the last argument, and deletes both when the program has ended.
/// Debug prints `Program { args: N, stdin: bool, script: bool, .. }`.
pub struct Program { pub program: PathBuf, pub args: Vec<OsString>, pub stdin: Option<String>, pub script: Option<String> }
pub enum ProgramRequest<'a> { AppleScript(&'a str), Shell(&'a str), AppleShortcut(&'a str), KeyboardMaestro(&'a str), Speak(&'a str) }
/// The program that does this here. The app runs it.
pub fn program_for(request: ProgramRequest<'_>) -> Result<Program, PlatformError>;
/// Ends a program started in its own process group, and everything it started.
pub fn kill_group(pid: u32);

pub enum SystemControl { SleepDisplay, LockScreen, Screensaver, MissionControl, ToggleMute, VolumeUp, VolumeDown, ToggleDarkMode, ScreenshotArea }
pub enum SystemPlan { Program(Program), CharChord { ch: char, mods: Modifiers } }
pub fn system_plan(control: SystemControl) -> Result<SystemPlan, PlatformError>;

pub enum Placement { LeftHalf, RightHalf, TopHalf, BottomHalf, Fill, Center, TopLeft, TopRight, BottomLeft, BottomRight }
/// Pure: where `placement` puts a window of `size` on a display's `work_area`. On an odd side the
/// right or bottom part gets the extra unit; `Center` shrinks a window larger than the work area.
pub fn placement_frame(placement: Placement, work_area: Rect, size: (i32, i32)) -> Rect;
/// Move the focused window of `pid` (needs input access).
pub fn place_window(pid: i32, placement: Placement) -> Result<(), PlatformError>;

/// Give the front back to the app `pid`, the one the wheel opened over, if Grape is the active app,
/// whichever of Grape's windows is key. `None` changes nothing. Queue it after the overlay's hide.
pub fn give_back_front(pid: Option<i32>);
/// The app in front right now; `None` when that is Grape itself, or nothing. On macOS it asks the
/// main thread and waits a second at most. Call from a worker thread.
pub fn front_app() -> Result<Option<FrontApp>, PlatformError>;
/// Wait up to `timeout` for `pid` to be the app in front, handing the front to `pid` whenever Grape
/// is the active app. Call from a worker thread, never the main one (there it looks once and returns).
pub fn ensure_front(pid: i32, timeout: Duration) -> bool;

pub enum PickKind { App, FileOrFolder }
pub struct Picked { pub path: PathBuf, pub folder: bool, pub app: Option<AppIdentity> }
pub struct AppIdentity { pub id: String, pub name: String }
pub fn pick_target(kind: PickKind, owner: Option<isize>) -> Result<Option<Picked>, PlatformError>;
```

### macOS notes for the implementer

- **Hot keys: Carbon** (`RegisterEventHotKey`), declared by hand in `macos/hotkey.rs` with `#[link(name = "Carbon", kind = "framework")]`. No new crate, no permission. One `InstallEventHandler` on `GetApplicationEventTarget()` for class `'keyb'` kinds 5 (pressed) and 6 (released); read the id with `GetEventParameter(event, '----', 'hkid', …)`. Carbon modifier masks: Command `0x0100`, Shift `0x0200`, Option `0x0800`, Control `0x1000`. `EventHotKeyID` and `EventTypeSpec` are two `u32` each, `#[repr(C)]`.
  - Everything Carbon runs on the main thread, and the monitor keeps its `EventHotKeyRef`s in main-thread state (the trait is `Send`; raw refs are not). `start` and `bind` hop there with a waiting `run_on_main` and return the result; the main thread never waits for the controller's thread, so that can't deadlock. It can stall, though: see "Grape's own modal panels" below. `stop` doesn't wait off the main thread: it queues its work there, in order with any later `start` or `bind`, because a waiting stop could deadlock Quit. One monitor owns the process-wide table; a second gets `PlatformError::AlreadyRunning`.
  - The pressed handler reads the cursor, the pressed buttons and the front app, then `try_send`s. On an Open press it also refreshes the screen cache, which takes the screens mutex briefly. It takes no lock a `bind` caller could hold while it waits. Each registration's id carries the signature `'GRPE'` and, in its top byte, what it is for, so the handler looks nothing up.
  - **Grape's own modal panels.** The pickers, Move's folder panel and Move's alert run `runModal()` inside a block on the main dispatch queue, and that queue serves no second block until the panel closes. A waiting hop made then (a `bind`, a `start`) waits as long. So an Open press while `NSApplication.modalWindow()` is set is **not signalled**: the handler logs one fixed line at debug level, `hot keys: Open ignored while a modal window is up` (the app logs at info level, so `Grape.log` doesn't show it), and still answers "handled", so the chord stays Grape's and the panel doesn't get it as a key. No wheel opens over a panel. The rule is the pure `mac_rules::passes_press(id, modal_up)`: Escape and Confirm presses and every release always pass.
  - **A hop that can't wait that long** uses `on_main_within(context, patience, work)` (`macos/shortcuts.rs`): the work is queued with `exec_async` and waited for `patience` at most. If the caller gives up first, the queued block never runs, so nothing stale happens once the panel closes (the race is the pure `mac_rules::HopClaim`). `ensure_front`'s hand-back, `send_char_chord`'s layout read and `front_app` use it. `bind`, `start`, `Track(false)` and Pause's bind thread still wait behind an open panel, and a press that lands just before a queued panel block starts still stalls: the root cause (panels inside a main-queue block) is unchanged.
  - Carbon matches modifiers exactly, and a registration almost never fails even when another app has the chord. A real failure is `PlatformError::Os { context: "RegisterEventHotKey", code }`; Settings words it "Grape couldn't use that shortcut. Try another." for a new chord and "macOS wouldn't give Grape that shortcut." for the switch (§8).
  - **`bind`'s failure rule:** the new chords are registered first; a refusal rolls them back and returns the error; the old chords are let go only after every new one registered. So a failed bind leaves `id` exactly as it was (the plan is the pure `mac_rules::bind_plan`).
  - A chord another id already holds is skipped, not an error: with the Open chord on the Esc key, `bind(Escape, subsets)` leaves that one subset to Open, where Carbon would have refused the duplicate and bound no Esc at all.
  - A failed unregister is logged and tried again on the next bind: a bare Esc or Return left registered would be dead everywhere until Grape quits.
- **Escape and Confirm** are hot keys too (Escape `53`, Return `36`), bound only while a Shortcuts wheel is open. Because Carbon matches exactly and the user may still be holding the hotkey's modifiers, each is bound once for **every subset of the Open chord's modifiers** (`Chord::with_modifier_subsets`). That is how Grape reads Esc without a key monitor. While bound they are Grape's: the app in front doesn't see them. Command+Option+Esc stays the system's.
- **The front app** is read in the pressed handler, on the main thread, before anything of Grape's shows: `NSWorkspace.frontmostApplication` → pid and `bundleIdentifier` (which may be missing). Grape itself in front → `None`.
- **The pointer** comes from polling `NSEvent.mouseLocation` and `NSEvent.pressedMouseButtons` on a `"grape-pointer"` thread every 8 ms while active (the drag poller's own trick; no permission; it sees a click the overlay swallows), flipped with `mac_rules::cursor_to_grape`. The thread is parked while off and holds an `NSProcessInfo` activity while active, as the drag poller does. The edge rule is the pure `mac_rules::PointerEdges`. If tap-to-click proves shorter than a poll on a Mac, the fallback is a local `NSEvent` *mouse* monitor on the main thread, which needs no permission.
- **Input access** is Accessibility: `AXIsProcessTrusted` / `AXIsProcessTrustedWithOptions` with `kAXTrustedCheckOptionPrompt`, declared by hand against `ApplicationServices`. `request_input_access(false)` shows the system's own prompt (which also puts Grape in the list). With `true` it also opens `x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility` with `/usr/bin/open`, and waits for it; only the Settings button asks for that, and only when a press finds Grape still untrusted.
  - Without access, `send_chord`, `send_char_chord` and `type_text` return `Ok` and nothing happens: macOS drops the events silently. That is why the runner checks `input_access()` first (§9). `place_window` does fail then.
  - The DMG is ad-hoc signed (D45), and macOS ties an ad-hoc app's grant to that exact build. After an update the list still shows Grape switched on while the check says no. The copy says so: "If Grape is already in that list, remove it with − and add it again."
- **Posting keys:** `objc2-core-graphics`' `CGEvent`, with the manifest features `CGEvent`, `CGEventTypes`, `CGRemoteOperation` and `CGEventSource` (no new package; not `post_to_pid`, which needs `libc` there). Events come from a private source (`CGEventSource::new(CGEventSourceStateID::Private)`), their flags are the event's own minus Shift, Control, Option and Command, plus the chord's, and key down then key up are posted at the HID tap 4 ms apart. Flag masks: Shift `0x20000`, Control `0x40000`, Option `0x80000`, Command `0x100000`. An event that can't be made fails the step.
- **Character chords:** `send_char_chord` finds the virtual key that types `ch` on the current layout (`TISCopyCurrentKeyboardLayoutInputSource`, `kTISPropertyUnicodeKeyLayoutData`, `UCKeyTranslate` over keys 0–127 with no dead keys; declared by hand; main thread) and falls back to `mac_rules`' US table when the layout has no data or no such key. The read waits for the main thread 1 s at most (`on_main_within`); when it doesn't answer (one of Grape's modal panels is up) the chord fails with `Os { context: "reading the keyboard layout" }` and nothing is pressed, since a guess from the US table could press another key on Dvorak or AZERTY. For a chord with Command it first reads what each key types with Command held, then with nothing held, then the US table: that is what makes Dvorak-QWERTY and Cyrillic layouts come out right. The number pad is never used: on AZERTY only the pad types a bare `4`, and Command+Shift+pad-4 is not the screenshot chord.
- **Typing:** `type_text` sends the pieces `mac_rules::text_chunks` cuts (`TextPiece::{Units, Return, Tab}`): chunks of at most 20 UTF-16 units that never end inside a surrogate pair, just before something that joins the character before it (`joins_previous`: a combining mark, a variation selector, a zero-width joiner, an emoji's skin tone U+1F3FB–1F3FF, a tag character U+E0020–E007F), just after a joiner, or between the two regional indicators of a flag (so an accented letter, a skin-toned family emoji, a keycap and a flag stay whole); only a run of such characters longer than a chunk is cut. The combining marks are a hand table (`is_combining_mark`): the general blocks (U+0300, U+1AB0, U+1DC0, U+20D0 with the keycap mark U+20E3, U+FE20) and the marks of Cyrillic, Hebrew, Arabic, the Indic scripts from Devanagari to Sinhala, Thai, Lao and kana; marks only, never whole blocks, or Indic text couldn't be cut at all. Not covered: Hangul jamo, prepended marks, Tibetan, Myanmar, Khmer and Syriac marks, and a cut after a virama inside a conjunct. Each chunk is a key down and key up on virtual key 0 from the private source with all four modifier flags cleared, 3 ms between events. `\n` is Return and `\t` is Tab. About 6 ms a piece, so 4 000 units take about 1.2 s.
- **Windows of other apps:** `AXUIElementCreateApplication(pid)`, `AXUIElementSetMessagingTimeout(app, 1 s)`, `"AXFocusedWindow"` (else `"AXMainWindow"`), read `"AXPosition"` and `"AXSize"` (`AXValueGetValue`, CGPoint type 1, CGSize type 2), `placement_frame` on the display holding the window's center, then set position, size, position. A window already the target size gets only the position, so a fixed-size window can be centered. The 1 s timeout is set on the window element too, and CF type ids are checked before a value is read. AX points have a top-left origin, the same as Grape's. A window that refuses is a step failure.
- **Programs** (`mac_rules`, each a pure builder with tests). A script is never an argument, so it never shows in the process list:
  - AppleScript: `/usr/bin/osascript -` with the source on stdin. `osascript` reads the whole script before it runs any of it.
  - Shell: `/bin/zsh -l <file>`: `Program::script` holds the source, `stdin` is `None`, and the runner writes the private file and appends its path (§9). A login shell, so the user's own `PATH`. Never `zsh -l -s` with the source on stdin: a shell reads that a command at a time and its commands inherit it, so one that reads its input (`cat`, `read`, `ssh`, `ffmpeg`) swallowed the rest of the script. This way they read `/dev/null`. Checked with zsh 5.9 on Linux: a file holding `cat` then `echo after`, run as `zsh -l <file> </dev/null`, prints `after`, and `exit 3` gives 3.
  - Apple Shortcut: `/usr/bin/shortcuts run <name>`.
  - Keyboard Maestro: `/usr/bin/osascript -e 'on run argv' -e 'tell application "Keyboard Maestro Engine" to do script (item 1 of argv)' -e 'end run' -- <name>`. The name is an argument, never spliced into the script.
  - Speak: `/usr/bin/say <text>`; a text starting with `-` gets a leading space.
  - Open (`open_programs`): for an app with an id, `/usr/bin/open -b <id>` and then `/usr/bin/open -a <path>`; for one without, the second alone; `/usr/bin/open <path>`; `/usr/bin/open <url>`. Each path, id and URL is one argument, and none has a script or input. The runner runs them in order like any program (§9), so an open has a time limit and Pause and Quit reach it. `open_target` is kept, loops over the same list with a plain wait and no timeout, and no step uses it.
- **`kill_group`** sends SIGTERM to the group, and SIGKILL 500 ms later from a `"grape-kill"` thread, only if the SIGTERM was delivered. It does nothing for pid 0, pid 1, a pid above `i32::MAX` or Grape's own group.
- **System controls:** Sleep display `/usr/bin/pmset displaysleepnow`; Screensaver `open -a ScreenSaverEngine`; Mission Control `open -a "Mission Control"`; mute, volume up, volume down and dark mode are Grape's own scripts on stdin through `osascript -`, like any AppleScript step. Mute is one line. Volume up and down are three (read the level, add or take 6, clamp to 0–100, set it), since nobody could check from Linux what `set volume output volume 106` does. Mute and volume ask for nothing; dark mode goes through `System Events` (one Automation prompt); Lock screen is the character chord Control+Command+`q`; Screenshot area is Command+Shift+`4`.
- **`give_back_front(pid)`** is one closure on the main thread. With `Some(pid)` it calls `hand_back` for that app, the call `ensure_front` makes: the front goes to the app the wheel opened over whenever Grape is the active app, whichever of Grape's windows is key. That matters with Settings open behind the other app: a click on the wheel activates Grape and makes Settings key again, and the toast's rule (`hand_back_activation`, a no-op while a Grape window is key) then gave nothing back. If that pid has quit, it falls back to `hand_back_activation()`. `None` or a pid of 0 or less also falls back to `hand_back_activation()` (the wheel opened over Grape itself, or nothing was clicked). Any thread but the main one does nothing. The app queues it with `run_on_main_thread`, the same queue as the overlay's hide, so it runs after it; which pid it gets is §5's click rule.
- **`front_app()`** hops to the main thread with `on_main_within` and reads `frontmostApplication` as the pressed handler does (they share `front_app_now`): `None` while Grape is in front. A main thread that doesn't answer within 1 s is `Os { context: "reading the app in front" }`.
- **`ensure_front`** loops for the whole timeout: every 10 ms it finishes with `true` when `frontmostApplication` is `pid`, and otherwise, if Grape is the active app, hops to the main thread to hand the front to `pid` itself (not to "the app last used"). It reads "Grape is active" off the main thread and hops only then. The hop waits only for what is left of the timeout (`on_main_within`), and a hand-back that came too late is never made, so the call can't outlive its timeout behind a modal panel. Called on the main thread it looks once and returns. It never hops for a `pid` of 0 or less.
- **Pickers:** `NSOpenPanel`, as `pick_folder` does, on a blocking worker like Settings' folder picker. Apps start in `/Applications` and allow only `.app` (`setAllowedFileTypes` under `#[allow(deprecated)]`: the newer call would add a package). The identity is `NSBundle`'s `bundleIdentifier` (feature `NSBundle`) and `NSFileManager`'s display name, with a trailing `.app` taken off (Finder adds it when it shows all extensions). A bundle with no id gives `app: None`. The panels say "Choose an app" and "Choose a file or a folder", with the button "Choose".
- **The clock:** `local_date_time` uses `libc`'s `localtime_r`, already linked.
- `Info.plist` gains `NSAppleEventsUsageDescription` (AppleScript steps that talk to other apps), in Grape's words. When M9 turns the hardened runtime on, it needs the `com.apple.security.automation.apple-events` entitlement too.
- Unit tests cover `mac_rules` and the pure parts, on every OS. The Mac-only tests in `macos/{hotkey,input,shortcuts}.rs` make a few real calls (a pointer thread, the clock, `AXIsProcessTrusted`, `/usr/bin/true` and `/bin/sh` as children) and run only in CI's macOS job. Nothing in a test registers a hot key, posts a key, opens a panel or calls `run_on_main`. One test does queue a block with `on_main_within` and checks that it gives up: nothing serves the main queue under `cargo test`, as nothing does while a modal panel is up.

## 5. The wheel session (`shortcuts/session.rs`, driven by `controller.rs`)

New controller inputs: `Input::Hotkey(HotkeySignal)`, `Input::Pointer(PointerSignal)`, `Input::Shortcuts(Arc<Snapshot>)` (the enabled flag, where it opens and the menus; sent at start and after every change with a waiting `send`, never the dropping `try_send`), `Input::RunEnded` (the runner is free again; the app answers every `RunShortcut` with one, whether the run started or not, on a waiting send).

New effects. The first three are emitted **only** from `State::Shortcuts`, the fourth only from Idle; a drag wheel's effect sequences don't change (two drag flows are pinned effect for effect in the controller's tests):
- `Effect::Track(bool)`: start or stop the pointer watch, and bind or unbind Escape and Confirm.
- `Effect::GiveBack(Option<i32>)`: queue `give_back_front(pid)` after the overlay's hide. The pid is the session's `give_back()`: the app the wheel opened over, and only if a button went down on the wheel's display while it was open. With `None` the call is still queued, and the platform falls back to the toast's rule (the app last used, and only if Grape is active with no key window).
- `Effect::RunShortcut(ShortcutRun)`.
- `Effect::StillRunning { anchor, display }`: the hotkey went down while a run is in flight. The app shows "Still running {name}" at `anchor`, which is where the wheel would have opened. A toast needs a place, which a log line can't carry.

`Open`, `ShowOverlay`, `Highlight`, `Close` and `HideOverlay` are reused, so the haptic comes for free; the tick needs §6's page change.

New state: `State::Shortcuts(…)`, holding `seq` (from the controller's own counter, shared with the drag wheels; the session never numbers a wheel), the display, the anchor, the front app, the trail of open groups, the current ring's items, the highlight, the last pointer position, `sticky`, whether a button-down was seen, whether one was seen on the wheel's own display (`clicked`), whether it was painted, the press time and the last event time. The anchor math (logical offsets, the hit test, the global anchor) is one helper both wheel states use.

| In state | Input | Result |
|---|---|---|
| Idle, on, not paused, no run in flight | `Pressed(Open, ctx)` with a cursor, a display and no button down | choose the menu for `ctx.front`; anchor = the cursor (or the work area's center), clamped; `Open`, `ShowOverlay`, `Track(true)`; → Shortcuts, not sticky. Nothing is highlighted until the pointer watch's first `Moved` |
| Idle | `Pressed(Open, …)` while off or paused, with a button down, or with no cursor or display | nothing (a log line) |
| Idle | `Pressed(Open, …)` while a run is in flight | `StillRunning` and a log line; no wheel. The guards are checked in this order: off, paused, button down, no cursor, no display, then a run in flight. So a press during a run with a button down only logs |
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

- **One teardown.** `WheelController::close(reason, next)` handles both wheels. For `State::Shortcuts` it emits `Close`, `HideOverlay`, `Track(false)`, `GiveBack(session.give_back())`, in that order. No code path may replace a `Shortcuts` state except through it; the wildcard arm of `DragSignal::Ended` becomes explicit per state. `next_deadline()` and `tick()` cover `State::Shortcuts` (the paint deadline, then `last_event + WATCHDOG`).
- **Pick, a shortcut:** the teardown with reason `picked`, then `RunShortcut`. → Idle. A shortcut with nothing to run (no steps, or only unset ones) is picked the same way: only a `ShortcutRun` carries the anchor and display its toast needs, so the runner answers `NothingToRun` and the app shows "Nothing to run yet". It counts as in flight until `RunEnded`, like any run.
- **A click** is hit-tested where the button went up, not by the highlight. A button-down only records that a press was seen, and whether it was on the wheel's display.
- **Giving the front back.** `Session::give_back()` is `Some(pid)` only when the wheel opened over another app and a button went down on the wheel's display while it was open: only a click on the overlay makes Grape the active app. Without a click Grape took nothing, and the user may be in another app on purpose by the time a watchdog closes the wheel, so `GiveBack(None)`. A click on a second display went to whatever is there (Grape's own Settings window, say), so it doesn't count either. With Grape in front at the press there is no pid.
- **A press over one of Grape's modal panels** never reaches the controller: the platform doesn't signal it (§4, "Grape's own modal panels").
- **Pick or leave a group:** change the trail, clear the highlight, send `Open` with the same `seq` (an update: no animation), then hit-test the last pointer position and send `Highlight` if it is on a segment. No `Highlight(None)` is sent: the page drops its highlight on a redraw. Become sticky.
- **Cancel / close:** the teardown. → Idle.
- **An empty menu** opens with no segments and shows the "nothing here yet" message. It closes like any other.
- `last_event` is refreshed by every pointer signal (a button-up with no button-down seen included) and by a hotkey signal that does something (Confirm with nothing highlighted counts). It isn't refreshed by `Painted`, `Configure`, `Shortcuts` or a hotkey signal that is ignored. `WATCHDOG` is handed to the session, so `session.rs` needs nothing from `controller.rs`.
- The Open hot key is **bound only while the feature is on and Grape isn't paused**, so a paused or switched-off Grape never swallows the chord. The "off or paused" row is a guard for a press already in flight. Until the first `Input::Shortcuts` arrives the wheel counts as off.
- **Esc that can't be bound:** when `Track(true)` can't bind Escape, the app hands the controller a `Pressed(Escape)` at once, so the wheel closes with reason `escape` (golden rule 4). The app's `apply` returns such follow-up inputs, and the controller's thread handles them before the next input, because it can't wait on its own queue. A Return that can't be bound is only logged.
- `Configure` and `Shortcuts(…)` each emit one log line ("shortcuts wheel: on, 1 menu(s)").
- Close reasons used: `picked`, `cancel` (clicks, a late release over nothing, a second press, any drag signal that ends it), `escape`, `paused`, `watchdog` (both deadlines; the log line says which).
- Hit testing is `grape_core::hit_test` on logical offsets from the anchor, exactly as for drags.

`ShortcutRun { seq, name, steps, front: Option<FrontApp>, anchor, display }`. `ShortcutRun`, `Snapshot`, `FrontApp` and every model type that holds something the user typed implement `Debug` **by hand**, printing kinds and counts only (`Input` and `Effect` derive `Debug`, and a derived one would put a script in the log the first time someone printed an effect). A test checks a sample holding a marker string never prints it. The pick's log line says "segment {index}", never the name.

## 6. The overlay contract

`wheel://open` keeps its keys. `content` gains one kind, and `mode` is `"shortcuts"` exactly when the content kind is:

```json
{"kind":"shortcuts","dense":false,"trail":["Edit"],"segments":[
  {"index":0,"center":1.5707963267948966,"start":3.141592653589793,"end":0.0,"labelX":0.0,"labelY":-84.0,
   "title":"Save","icon":{"kind":"emoji","value":"💾"},"group":false}]}
```

- `icon` is `{"kind":"none"}`, `{"kind":"emoji","value":"…"}` or `{"kind":"glyph","value":"<id>"}`. Glyph ids come from one list shared by Rust and the UI (`GLYPHS`, 24 ids): Grape's own 24×24 stroke drawings. An id the page doesn't know, or an empty emoji, draws no icon and the segment falls back to its label. An emoji icon draws only the start of its value: one emoji, or two letters.
- `segments` may be empty: the page then shows the empty message. `trail` is the open groups' names, outermost first.
- `title` is the shortcut's name as typed. Guards check user-typed strings (`title`, `trail`, an emoji's `value`) for their type only, never their length: a guard that disagreed with Rust about length would drop the open while Rust held Esc.
- `wheel://close` gains the reason `"picked"`.
- New fixtures: `wheel-open-shortcuts.json`, `wheel-open-shortcuts-empty.json`, `wheel-close-picked.json`. The first shows a group segment under a one-name trail, which the two-level rule means a real session never sends; the guard must not reject it.
- The page calls `wheel_painted` after every open, a group's redraw with the same `seq` included. "Log it once" is Rust's rule.

Rust (`events.rs`): `pub enum UiMode { Conversions, Tools, Actions, Shortcuts }` (camelCase) with `From<WheelMode>` and `PartialEq<WheelMode>`, used only as `WheelOpen.mode`; `OpenWheel.mode`, `DropReport.mode`, `KeyMap::mode_for` and the panel keep `grape_core::WheelMode`. `UiContent::Shortcuts { dense, trail, segments: Vec<UiShortcutSegment> }` with `UiShortcutSegment { #[serde(flatten)] geo: Segment, title, icon: UiIcon, group }`, built by `UiContent::shortcuts(trail, items)`. `UiContent`, `UiShortcutSegment` and `UiIcon` write `Debug` by hand (§5). `CloseReason::Picked`. The three existing `wheel-open-*` fixtures must still serialize byte for byte.

The page (forced edits):
- `shared/events.ts`: `WheelMode` and `WHEEL_MODES`, `CloseReason` and `CLOSE_REASONS`, a `WheelIcon` union, a `ShortcutSegment` (keys `index, center, start, end, labelX, labelY, title, icon, group`; no `entry`), a `"shortcuts"` case in `isWheelContent` that allows zero segments, and the mode/kind pairing.
- `shared/events.test.ts`: the three fixtures in `CASES`, `"picked"` in the reason list, and `shortcuts-view-*.json` and `shortcuts-step-*.json` excluded from the glob the way `settings-view-*` is.
- `wheel/scene.ts`: `validHighlight` accepts the new kind (today it answers `null` for anything but `entries`, so nothing would highlight or tick), `buildScene` and `WheelScene` gain the kind, and the center shows the hovered title, else the last `trail` name.
- `wheel/copy.ts`: `MODE_NAMES`, `wheelLabel`, and the empty message, in Grape's words: "No shortcuts here yet. Add some in Settings."
- Labels: a new `shortcutLabelLines(title, dense, maxLines = 2)` that breaks a word longer than the line and ends a cut line with "…" (`wrapLines` and its tests stay as they are). It breaks between grapheme clusters (`Intl.Segmenter`) where the webview has them and between code points where it doesn't, and counts width in letter widths, with CJK, kana, hangul, fullwidth forms and emoji counting 2. A segment with no icon shows up to two lines; one with an icon shows the icon over one line; at `dense` (7 or more) it shows the icon alone, and the title shows in the center on hover and in the segment's `aria-label`. A Shortcuts wheel carries **no `<title>`**, on its segments or on the SVG root: it can stay open under a real pointer, and the webview would pop a tooltip over it. Its names are in `aria-label` alone; the drag wheels keep both. A group's segment carries a small mark: a short arc on the backdrop's rim, outside the segment, so a long label never crosses it. Its spoken name ends ", group", and the wheel's own `aria-label` names the open group ("Shortcuts wheel, Edit, 3 choices").
- The page still decides nothing and reads no pointer: Rust does. But a sticky wheel is the first time the overlay's page gets real pointer events, so it cancels `contextmenu`, `mousedown` and `dragstart` (no WebKit menu, selection or page drag).
- Tests: a highlight on a shortcuts payload paints and ticks; the labels for a 24-unit unbroken name, a CJK name and an emoji name; no `<title>` anywhere in a Shortcuts wheel, and both still on a drag wheel.

## 7. The model and `shortcuts.json`

**Limits** (every length is UTF-16 code units, counted in Rust with `encode_utf16().count()`, as the DOM's `maxlength` is specified to count; whether WKWebView counts typed emoji that way or by whole characters is a Mac check, and either way text Rust refuses for its length stays in a step's field, §8): 12 shortcuts in a ring (`MAX_ITEMS`); groups hold shortcuts with steps only (two levels); 16 steps (`MAX_STEPS`); names 1–24 after trimming, with no control characters or line breaks; an emoji icon's text 1–16 with no whitespace or controls; text fields 4 000; scripts 20 000; URLs 2 048; a delay 10 ms–60 s; 40 menus (Everywhere and 39 apps); the file 1 MiB. Not in the view's `limits`, because the page never types them: an app id and an app's display name are 1–255 with no control characters, and a picked path is at most 4 096, one line, and absolute by its text.

```json
{"version":1,"enabled":false,"hotkey":{"key":"KeyG","shift":false,"ctrl":true,"alt":true,"meta":false},
 "opensAt":"cursor",
 "menus":[{"scope":{"kind":"everywhere"},"withEverywhere":false,"items":[
   {"name":"Copy","icon":{"kind":"glyph","value":"clipboard"},"kind":"steps","steps":[
     {"kind":"clipboard","action":"copy"}]}]}]}
```

Ids are **not** in the file. Rust numbers menus, shortcuts and steps from 1 when it loads and keeps the counter in memory; ids are unique across the whole file and appear only in the view.

Step kinds (stored form): `keys {chord|null}`, `typeText {text}`, `clipboard {action: copy|paste|clear}`, `openApp {path|null, bundleId|null, name|null}`, `openFile {path|null, folder}`, `openUrl {url}`, `appleShortcut {name}`, `appleScript {source}`, `shell {source}`, `keyboardMaestro {name}`, `window {placement}`, `system {control}`, `notify {text}`, `speak {text}`, `delay {ms}`. An app menu's scope is `{"kind":"app","bundleId":"…","name":"…"}`.

- **Menu choice:** the menu whose scope is the front app's id, else Everywhere (also when the front app has no id). An app menu with `withEverywhere` on ends with one extra group, "Everywhere", holding the Everywhere menu's shortcuts (its groups left out: two levels only); it is left out when that would pass 12, and the view's `note` says so ("Everywhere didn't fit in this menu.") when the switch is turned on for a full menu, and on the `shortcuts_add_item` that fills the ring while the switch is on (`add_item_noting`; not for an add into a group, with the switch off, or in the Everywhere menu).
- **Text variables** exist in `typeText` only: `{date}` (2026-11-23), `{time}` (14:05), `{clipboard}`. Unknown braces stay as typed, and so do `{date}` and `{time}` when the clock is unknown. Expansion is one pass (a clipboard holding `{date}` isn't expanded again) and a pure function of the text and the three values.
  - The clipboard's value has every control character removed, with `\r`, `\n` and `\t` each turned into one space, and is cut so the whole expanded text stays within 4 000: copied text typed into a terminal must not run as commands. No text on the clipboard expands to nothing. The pasteboard is read only when the text holds `{clipboard}`.
  - Stored `typeText` itself allows `\n` and `\t` only; `\r\n` is stored as `\n`; any other control character is refused. The same one rule holds for `notify`, `speak`, `appleScript` and `shell`, so a lone `\r` in a script is refused.
  - U+2028 and U+2029 from the clipboard become a space too. Several `{clipboard}` in one text share the room that's left equally. `{date}` and `{time}` are never cut, so a text full of `{date}` can pass 4 000 once expanded.
  - At run time an expanded text that is empty types nothing, and a clipboard that can't be read fails the step rather than typing the text with a hole in it.
- **URLs** match `^[a-z][a-z0-9+.-]{0,31}:` followed by at least one character, with nothing at or below U+0020 and nothing in U+007F–U+009F. `file` is refused because the page never sends a path (D19); `javascript` and `data` because they open nothing useful. Any other scheme opens whatever app registered it: the user typed it, and the rule is a syntax check, not a safety boundary.
- **Names** for Apple Shortcuts and Keyboard Maestro macros: no control characters, not starting with `-`, and at most 4 000 (the page uses `limits.text` for them).
- **Trimming:** item names, an emoji icon's text, URLs, Apple Shortcut and Keyboard Maestro names and app display names are trimmed before they are checked and stored, on edit and on load. The page may send untrimmed text and gets the stored value back in the view.
- **Empty means "none yet".** A save follows every `add_step`, so a fresh step of any kind must be storable. A Keys step with no chord, an open step with no target, and an empty URL, Apple Shortcut name, Keyboard Maestro name, script, typed text, notice or speech are all valid stored states, and so is a shortcut with no steps. `StepKind::is_set()` is false for them, and unset steps are skipped at run time (`runnable(steps)` false → "Nothing to run yet").
- **Starter set:** the in-memory default for a missing file, on the Mac: Everywhere holds Copy, Paste, Screenshot, Mission Control, Mute and Lock screen (clipboard and system steps only, so they are right on every keyboard layout), each with a glyph. Nothing is written until the first change. The trailing "Everywhere" group of an app's menu has the glyph `stack` and shows even when it would be empty.
- While the feature isn't available on this OS, nothing is loaded, created or written, and every changing command is refused with one sentence. "Available" means the OS has the wheel (`model::available`), nothing else: a Mac whose hot keys couldn't be made or started still loads the file and shows its menus (§8).
- **Loading is total**, in two passes. First `Top { version, enabled, hotkey, opensAt, menus: Vec<Value> }` with the scalars as raw values: a file that isn't an object, has no `version` or no `menus` array, has a newer `version`, or is over 2 MiB is moved aside as `shortcuts.unreadable.json` (the settings file's rule, with its own stem) and the defaults are used. `enabled`, `hotkey` and `opensAt` each fall back alone (a hotkey that names an unknown key or fails `hotkey_problem` becomes none, and `enabled` false). Then each menu and each shortcut is read by itself, with unknown fields refused. A bad step drops its **whole shortcut** (running the rest of it would do something else), and a Keys step equal to the file's hotkey is a bad step; a bad shortcut drops alone, and one whose glyph id this build doesn't know is a bad shortcut; a second Everywhere menu, a second menu for one app, a group inside a group and anything past the limits are dropped; a missing Everywhere menu is added empty, first; `withEverywhere: true` on the Everywhere menu is read as false. App ids are matched exactly, case included.
- When a load dropped anything, the file is copied to `shortcuts.unreadable.json` (the next free name) before the first save. "Anything" includes a top-level key this build doesn't know and `enabled: true` with no usable hotkey. The view's `note` says "Some shortcuts couldn't be read and were left out." only when a menu or a shortcut was left out; after a file moved aside whole it says "Grape couldn't read your shortcuts and started fresh. The old file is kept." The note goes out with the first `shortcuts_get` only.
- A file that couldn't be moved aside is never written over: saves are blocked for that launch, and each change answers "Couldn't save your shortcuts."
- **Nothing a user typed is ever logged:** not names, text, scripts, URLs or paths, and never a serde message (it quotes values). The log counts drops by kind and gives the error's class, line and column, as `settings.rs`' `parse_problem` does.
- **Saving:** the path is `app.path().resolve("shortcuts.json", BaseDirectory::AppData)`; no store is opened on it. Create the folder, write `shortcuts.json.tmp` (mode 0600 on Unix), `sync_all`, then rename it over the file (retrying a few times on a Windows `PermissionDenied`). After every change that changes something: setting a value to what it already is answers the view without saving. A failed save still applies the change and says "Couldn't save your shortcuts."
- **The size rule.** A change whose stored form would pass 1 MiB **and** is larger than the one before it is refused before anything is created: "That's more than Grape can save. Remove something first." (`model::size_allows`). A change that leaves the config over 1 MiB but no larger than it was goes through. That case exists because the two limits count different things: a load takes a file of up to 2 MiB of raw bytes, and a save counts Grape's compact form, so a file made by hand or merged from two can load with more in it than Grape would write. Before, such a config refused every edit, the removals the sentence asks for included. The `Store` carries `largest`: 1 MiB, or the stored size as loaded or last saved if that is more. A save over 1 MiB is written only when it is no larger than that, and once a save lands under 1 MiB the limit is 1 MiB again. The load limit stays 2 MiB: an equal limit would move aside a padded file whose compact form fits.
- The file is its own, not a key in `settings.json`: a broken one never takes the other with it.
- **Plain text:** the file, its `.tmp` and any moved-aside copy hold typed text and scripts in the clear, and the file is as trusted as the user's own account: anything that can write it can make Grape act. D47 records both as accepted, and the Settings section says "Shortcuts are saved as plain text on this Mac. Don't put passwords in them."

## 8. Settings commands and view

All granted to the Settings window only (each in `build.rs`' list, `generate_handler!` and `capabilities/settings.json`), each answering `Result<ShortcutsView, String>`. The page never sends a path: apps, files and folders are picked on the Rust side. It does send what the user typed (names, text, scripts, URLs); D47 says so. **No command runs a shortcut.**

| Command | Args | Notes |
|---|---|---|
| `shortcuts_get` | `{}` | |
| `shortcuts_set_enabled` | `{on}` | Refused while there is no hotkey ("Choose a key combination for the wheel first."). Waits for the bind; a bind the OS refuses leaves the switch off and is refused with "macOS wouldn't give Grape that shortcut." On a Mac whose hot keys never started in this launch, switching on is refused with "Grape couldn't start the wheel's key combination. Quit Grape and open it again." |
| `shortcuts_set_hotkey` | `{chord\|null}` | Waits for the bind; a failure leaves the old chord bound and is refused with "Grape couldn't use that shortcut. Try another." A chord a Keys step presses is refused (§3). `null` also turns the feature off. The page sends it once, from the "Set shortcut" button (below). |
| `shortcuts_set_opens_at` | `{at: "cursor"\|"center"}` | |
| `shortcuts_request_access` | `{}` | The system prompt; the settings pane too when a later press finds Grape still untrusted. "Later" counts a run's own ask: the runner and this button share one once-per-launch flag (`run::first_access_request`), so the prompt isn't repeated. Does nothing but answer the view when access is granted. |
| `shortcuts_add_menu` | `{}` | Opens the app picker. An app that has a menu already, an app with no bundle id, or Grape itself (by `app.config().identifier`) is refused. A cancelled picker answers the unchanged view. |
| `shortcuts_remove_menu` | `{menu}` | Everywhere can't be removed. |
| `shortcuts_set_menu_everywhere` | `{menu, on}` | |
| `shortcuts_add_item` | `{menu, group\|null, kind: "steps"\|"group"}` | Rust names it "New shortcut" or "New group". The view's `note` is "Everywhere didn't fit in this menu." when this add filled an app menu's ring while its Everywhere switch is on (§7). |
| `shortcuts_set_item` | `{item, name, icon}` | |
| `shortcuts_remove_item` | `{item}` | |
| `shortcuts_move_item` | `{item, to}` | `to` is a 0-based place in the item's own ring or group; out of range is refused. No moves into or out of a group in this version. |
| `shortcuts_add_step` | `{item, kind}` | |
| `shortcuts_set_step` | `{step, value}` | `value` is a `StepEdit`. |
| `shortcuts_remove_step` | `{step}` | |
| `shortcuts_move_step` | `{step, to}` | Inside its own shortcut. |
| `shortcuts_pick_target` | `{step}` | An `openApp` step gets the app picker; an `openFile` step one panel for files and folders (`folder` comes from what was picked). |

One picker at a time, shared by `add_menu` and `pick_target`: a second ask while one is open answers the unchanged view with no error. Settings' own folder picker keeps its separate flag, so a folder picker and an app picker could in principle be asked for at once. A picker that can't open answers "Couldn't open the picker."

Every command is async and a thin shell over one model edit: clone the config, edit, refuse by the size rule (§7), bind if the switch or the chord changed (`bind_step`: without hot keys in this launch only switching the wheel on is refused, and every other edit, a new chord and switching off included, is applied and saved with nothing bound), commit, save, send the controller the new snapshot, answer the view. A command answers `Err(sentence)` both for a refusal (nothing changed) and for a failed save (the change applied); the page calls `shortcuts_get` again to tell which, as Settings does.

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

- `hotkey` is `null` when none is set. `binding` is `"off"` (switched off, no hotkey, not available, or Grape paused: Open really is unbound then), `"bound"` or `"failed"`. Since a refused bind refuses the change, `"failed"` shows when a bind at launch or after a resume was refused, and for the whole launch on a Mac whose hot keys could not be made or started (`shown_binding`: also while the wheel is off or Grape is paused, since only opening Grape again mends it). The page words `"failed"` "macOS wouldn't give Grape that shortcut. Try another, or quit Grape and open it again." `access` is `"granted"`, `"denied"` or `"notNeeded"`.
- `available` is a plain "this OS has the wheel" (`model::available`). A Mac whose hot keys failed still reports `true`, loads `shortcuts.json`, shows its menus and takes every edit but switching the wheel on (the refusal in the table above); `wheel.rs` sends the controller its snapshot even when the sources didn't start. Only an OS without the wheel reports `false`. The shape of the view didn't change for this, and no fixture was regenerated.
- `note` is a sentence about the change that made this view (shortcuts left out at load, "Everywhere didn't fit in this menu." from the switch or from the add that filled the ring), `null` otherwise.
- An app menu's scope is `{"kind":"app","name":"Safari"}`: the name only. `openApp` and `openFile` steps carry `name` (the display name, or `null` until picked) and never a path; an `openFile` step also carries `folder`. A Keys step carries `chord` and its `label`, both `null` until one is chosen.
- **Entering a chord** is four labelled checkboxes and one `<select>` of keys, filled from `keys` and `modifiers`, with "None" first. Not a keydown recorder: the webview can't see a chord Carbon already took, and Command+W would close the window. The two places a chord is entered differ:
  - **The wheel's hotkey** is a draft. Nothing is sent while the boxes and the key are changed, so Rust never binds a chord the user is only passing through, and none is refused mid-edit. A **"Set shortcut"** button in the same group (`aria-label` "Set shortcut for the wheel") is `disabled` unless the draft differs from the chord Rust holds, and sends `shortcuts_set_hotkey` once; a second click while that is in flight is ignored. Key "None" plus the button sends `{chord: null}`. A chord Rust refuses keeps the draft and the button on, with Rust's sentence in the status line. A draft survives redraws and re-reads, and `pagehide` and hidden never send it. When the button goes off while it has the focus, the focus moves to the key select.
  - **A Keys step** has no button: its editor sends each chord as it is made. Modifiers ticked while the key is "None" are held unsent until a key is picked.
- Notify and Speak are text areas, since Rust allows `\n` in them. Selects send on every `change`.
- **The page** mounts with `mountShortcuts(root, { invoke })` into its own `<div id="shortcuts-root">` after `#settings-root`, with its own command queue, status line and `shortcuts_get`. `mountSettings` and its tests don't change. Inside the root: one `<section>` with an `h2`; menus and shortcuts as `div`s and `h3`s; every control with an id built from Rust's ids (`sc-item-7-name`, `sc-step-9-text`) and exactly one label; every button with an `aria-label` that contains its text.
- **Text fields** send on `change` (blur, or Enter in a one-line field), never on `input`. A field whose value differs from the last view isn't overwritten by a redraw until its own command has answered. A field tracks what Rust holds apart from what the control shows, so an edit back to the old value is still sent after Rust has moved on under typed text.
  - **Refused text stays.** A step's text, link, script, name or number that Rust refuses stays in the control with `aria-invalid="true"`, and a line under it (`<p class="sc-unsaved" id="sc-step-N-<key>-unsaved">`, in the control's `aria-describedby`) reads "Not saved. " and then Rust's sentence; the status line shows the sentence as before. Refused is told from a failed save after the re-read: the text is kept only when Rust still holds what it held. A reply that fails the guard keeps the text too. Refused text isn't sent again (by blur, Enter, `pagehide` or hidden) until the user changes it, and the mark goes when the field's next command succeeds or the text is put back to what Rust holds.
  - A shortcut's **Name and Emoji** fields are the exception: refused, they go back to the stored value under Rust's sentence. The two travel in each other's `shortcuts_set_item`, so a kept bad name would get every later icon change refused.
  - **On `pagehide` and when the page is hidden,** every command still waiting its turn, and then every unsent field, is invoked inside the handler, in the order made: the page may be gone before an answer comes. Answers are still read one at a time if the page lives on. The flush before a menu switch stays queued. Whether an `invoke` started in `pagehide` reaches Rust when the window is destroyed is a Mac check; Settings is still destroyed on close, and Quit with Settings open is not covered.
- **"Emoji…"** stays chosen, with its empty field shown, through blur, redraw and re-read, until an emoji is stored or another icon is chosen: a blur also comes when the window stops being key, as it does for the system's emoji picker. A rename meanwhile sends the stored icon. A refused emoji empties the field and leaves "Emoji…" chosen.
- **Move and Remove.** While a move is in flight, the Move buttons of that one list (a ring, a group, or one shortcut's steps) carry `aria-disabled="true"` and ignore clicks, since a second move would count its place from the old order; they are freed when it answers, refused or not. A Remove ignores a click whose `event.detail` is over 1 (the second click of a double click would land on the row that slid up) and is `aria-disabled` and inert while its own command is unanswered. A press from the keyboard reports 0 and works. `aria-disabled` and not `disabled`, so the pressed button keeps the focus.
- Rows are keyed by Rust's id (and kind) and never rebuilt while they hold focus; after a move the page focuses the moved row's button itself.
- The page reads `shortcuts_get` again when its window gets the focus or becomes visible, which is how it learns that access was granted in System Settings.
- A reply that fails the page's guard is dropped with one fixed `console.warn` naming the command and nothing else, as `mountSettings` does. A re-read on focus is quiet: it leaves the status line alone.
- On Windows for now `available` is `false`, the view holds one empty Everywhere menu and the default hotkey labelled "Ctrl+Alt+G", and the section is its heading and one line: "The Shortcuts wheel is on the Mac first. It isn't on this computer yet." The page doesn't re-read on focus there.
- Fixtures, in kebab-case: `shortcuts-view-macos.json`, `shortcuts-view-windows.json`, and 14 `shortcuts-step-*.json`, one per `StepEdit` shape plus the unset Keys step: `keys`, `keys-unset` (`{"kind":"keys","chord":null}`), `type-text`, `clipboard`, `open-url`, `apple-shortcut`, `apple-script`, `shell`, `keyboard-maestro`, `window`, `system`, `notify`, `speak`, `delay`. Each is read by both sides' tests, and Rust's compare byte for byte.
- Three lists hold the 17 command names and tests keep them equal and in one order: `build.rs`, `generate_handler!` in `lib.rs`, and `capabilities/settings.json`. No other capability may grant one.

## 9. Running a shortcut (`shortcuts/run.rs`)

- One run at a time, on a `"grape-shortcut"` thread. While one runs, the Open hot key shows "Still running {name}" and opens no wheel (`Effect::StillRunning`, §5). A drop guard frees the slot and sends `Input::RunEnded` even if a step panics; a run that doesn't start (paused, quitting, the slot busy, no thread) is answered with `RunEnded` too.
- `run_steps(run, actuator, cancel) -> Outcome` is pure sequencing over the `Actuator` trait (one method per need); `SystemActuator` is the real one. Since the review the trait has `open_programs` in place of `open_target`, a `front_app()` that answers a pid only (`None` is Grape or nothing), and a `run_program` that honours `Program::script`. `Outcome::{Finished, NothingToRun, Cancelled, Failed { step, kind, why }}` and `Failure` hold kinds and places only.
- Steps run in order. The first failure stops the run and shows an error toast near the wheel's anchor: "Couldn't finish {name}" with one plain sentence, chosen by the step's kind ("That app wouldn't open.", "The shell script stopped with an error.", …). Nothing to run is an info toast: "Nothing to run yet" / "Add a step to it in Settings." A step the OS has no way to do says "That step doesn't work on this computer yet." (every OS call off the Mac; Notify and Delay still work there).
- Before the first step that presses keys, types, copies, pastes or moves a window (Keys, Type text, Clipboard's copy and paste, Window, and a System control whose plan is a character chord; not Clipboard's clear, the open steps, programs, Notify or Delay), once per run (`Sequence::check`):
  - check `input_access()`. Denied → stop, call `request_input_access(false)` once per launch (the flag is a static the Settings button shares, §8), and say "Grape needs Accessibility access to press keys. Turn it on in System Settings, then try again." (the Settings section adds the remove-and-add sentence);
  - wait up to 500 ms for **every** modifier to be let go (`held_modifiers() == NONE`), not only the hotkey's own: `ShortcutRun` doesn't carry the hotkey, and the stricter wait is never wrong for posted keys. Still held → stop with "Let go of the keys first.";
- **The aim** (`enum Aim { Opener, InFront, App(pid) }`) is the app those steps are meant for, and whose window a Window step moves. `gate()` does the two checks above once, then resolves the aim and hands the step its pid:
  - `Opener`, where every run starts: the app that was in front when the wheel opened. `ensure_front(front pid, 400 ms)`; false → stop: "The app you were in didn't come back to the front." (a fixed sentence: `FrontApp` is a pid and an id, with no display name). With no front app (Grape's own window was in front) the step stops with "Click the app you want first." This is the only place `ensure_front` is called.
  - `InFront`, after a step that may have brought another app forward on purpose (`retarget()`): a successful Open app, Open file or folder, or Open link; an AppleScript, a shell script, an Apple Shortcut or a Keyboard Maestro macro; and a System control whose plan is a program. The next gated step is aimed at whatever app is in front by then. `settled_front()` asks the `Actuator`'s `front_app()` every 50 ms for up to 1.5 s and answers once the same pid, not Grape's, is seen at two looks in a row. If it never settles (Grape stays in front, or apps keep swapping) the step stops with `Failure::NoFrontApp`, "Click the app you want first.", and nothing is pressed. A `front_app()` that fails (on the Mac, a main thread that didn't answer within 1 s) fails the step with its kind's own sentence.
  - `App(pid)`, once resolved: later gated steps go straight through, as before, until the next `retarget()`.
  - Speak, Notify, Delay, Clipboard's clear and the character-chord System controls leave the aim alone.
  - So [Open app Notes, Press keys Command+N] presses the keys in Notes, [Copy, Open app Notes, Window Left half] moves Notes' window, and a run opened over Grape's own window that first opens something no longer fails at the later key step. Before, the gate insisted on the app from the press, and such a run always stopped with the "didn't come back" sentence.
  - **Still a race:** if `open` returns before a cold-launched app is in front, the old app is seen at both looks and gets the keys. A Wait step between the two is the remedy.
- `clipboard copy` is the character chord Command+`c`, then a wait of up to 500 ms for the pasteboard's change count to move, and then the run goes on either way: a Copy that never lands (nothing was selected) isn't a failure; `paste` is Command+`v`; `clear` empties the pasteboard. `notify` is Grape's own toast. `delay` sleeps in 20 ms slices so a cancel is quick.
- **Programs** all go through the one `run_program(program, cancel, limit)`: stdout and stderr to `/dev/null`; the home folder as working directory (no home folder → the program doesn't start, and the step fails); a process group of their own; the pid published for `cancel_run`; polled every 10 ms. Ending one is `kill_group`, then up to 600 ms for it to go, then `Child::kill`. A non-zero exit is a failure. Output is never read or logged. A run killed while macOS is asking the user to allow automation works on the next try.
  - **Input.** `Program::stdin` (AppleScript, and Grape's own system scripts) goes in on a pipe that Grape writes and closes, on a `"grape-shortcut-stdin"` thread so a long text can't hold the run past its limit. With no `stdin` the program reads `/dev/null`.
  - **A script file.** For `Program::script` (a shell step) the runner makes a new folder `<temp>/grape-shortcut-<pid>-<n>` (mode 0700; a `mkdir` that fails if anything is there, a link included; 16 names tried, so a folder an earlier Grape with this process id left is never used) and in it a file `script` (`create_new`, mode 0600, through `store::private_file`), and appends the file's absolute path as the last argument. A drop guard removes the file and the folder on every way out: ended, failed, timed out, cancelled, not started. A file that can't be written is `NotStarted`. Only an `io::ErrorKind` is logged, never the path. If Grape is killed or crashes mid-script the folder stays in the temp folder, with the script in clear text, until the next launch: `ShortcutsState::load` calls `run::sweep_script_folders`, which removes every real folder there named `grape-shortcut-…` that holds nothing but its `script`, and logs only how many went.
  - **Limits.** A script, an Apple Shortcut, a macro or a system program gets 60 s: "A step was still going after a minute, so Grape stopped it." An opener gets 10 s (`OPEN_LIMIT`): "That took too long to open, so Grape stopped waiting." Speak gets 10 minutes (`SPEAK_LIMIT`), since `say` returns only when it has finished and 4,000 characters are far more than a minute's worth: "Grape was still speaking after ten minutes, so it stopped." Pause and Quit end any of them sooner.
  - **The open steps** ask `open_programs` (§4) and run each program in turn: the first `Success` wins and sets the aim to `InFront`; `Failed` or `NotStarted` tries the next, and the step fails as its last opener did; `TimedOut` and `Cancelled` stop at once, since the next opener would only wait as long again. With no home folder an open step now fails, like any program.
- **Pause and Quit:** `ShortcutsState` holds the run's cancel flag and the child's pid. `shortcuts::cancel_run(app, why)` (`Why::{Paused, Quitting}`) is called from the tray's Quit, from Pause, and from both exit arms in `lib.rs`; it sets the flag, then kills the group itself, in that order. After `Quitting` no run starts again, and a run picked just as Grape is paused doesn't start either. The runner checks the flag before every step and before every hop to the main thread, and never hops after it is set. `RunEvent::Exit` waits for the run at most as long as it waits for jobs (2 s). `wheel::stop` also stops the hotkey monitor and the pointer watch, but only tries their lock: a bind in flight holds it while it waits for the main thread, and macOS lets go of the hot keys at exit anyway. For the same reason Pause rebinds Open on a `"grape-shortcuts-bind"` thread, never in the tray's handler. Pause stops a running shortcut, though it doesn't stop a conversion.
- A finished run shows nothing unless a step says so. The toast window is shared: a conversion's progress can replace a shortcut's toast.
- The log says which step *kinds* ran and how long, never their contents: `shortcut 7: ran [openUrl, keys] in 3 ms: step 2 (keys) failed: …`.

## 10. Order of work

1. `grape-platform`: `keys.rs`, `shortcuts.rs`, stubs, mock scripts, `mac_rules` tables (compiles on every OS, tests on Linux).
2. In parallel: the macOS implementation; the app's `model.rs` and `store.rs`; the event types and fixtures.
3. The session, the controller glue, the runner, the commands (`build.rs`, `generate_handler!`, `capabilities/settings.json`) and the wiring in `lib.rs`, `tray.rs` and `wheel.rs`.
4. The overlay drawing and the Settings section.
5. Docs: D47 (which amends D7 and the "no permission prompts" rule in `crates/grape-platform/CLAUDE.md` rather than leaving them contradicted), `INTERACTION_SPEC.md` §1, §8, §9 and a new §12, `ARCHITECTURE.md` (the "no pointer events" line, threads, settings, fixtures), `PLATFORM_MACOS.md`, `TESTING.md`, `ROADMAP.md` M10, `PRODUCT.md`, `README.md`, `m1-drag-wheel.md` §C, and the CLAUDE.md files.

All five steps are done, and so is a sixth: a code review on 2026-11-23, whose confirmed findings were fixed in the platform crate, the app and the pages, each with a test that fails without it wherever a test can run on Linux (the pure rules, the fake actuator, real `/bin/sh` children, the fake DOM). What the review left for a Mac is in D47's review bullet. Checked from Linux only: `cargo test` for the pure parts, the Mac and Windows clippy cross-checks in `START_HERE.md`, the UI's typecheck and tests. CI's macOS jobs compile and link the Carbon and AX declarations. Everything a person has to see has a `mac:` line in `TESTING.md` → "Shortcuts wheel, M10".
