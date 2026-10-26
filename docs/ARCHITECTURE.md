# Architecture

## Big picture

```
┌──────────────────────── Grape (Tauri 2 app, tray-only) ──────────────────────────┐
│                                                                                   │
│  grape-platform ──(DragSignal stream)──▶ app: WheelController ──▶ overlay window  │
│   macOS / Windows                         (state machine,           (ui/: SVG     │
│   drag + modifier + cursor                 INTERACTION_SPEC §1)      wheel)       │
│                                                 │                       │         │
│                                                 │◀── DragDropEvent ─────┘         │
│                                                 │    Enter/Over/Drop/Leave        │
│                                                 ▼                                 │
│                                   grape-core: targets(), hit_test(), plan()       │
│                                                 │  JobPlan                        │
│                                                 ▼                                 │
│                                   grape-engines: run(plan) ──▶ sidecars / crates  │
│                                                 │  progress events                │
│                                                 ▼                                 │
│                                   toast + output file next to source              │
└───────────────────────────────────────────────────────────────────────────────────┘
```

## Crates and their contracts

### `grape-core` (pure Rust: no I/O except path *strings*, no OS, no Tauri)
- `Format`: enum of the 45 inputs plus the one target-only format, `html` (46 in all, in `FORMAT_MATRIX.md` table order). Includes `from_extension()`, `from_file_name()`, `display_name()`, `category()` and `output_ext()`.
- `targets(format) -> &'static [Format]`: the conversion matrix across all tiers, in doc order. `targets_enabled(format, features)`, `pair_tier(from, to)` and `pairs(features)` apply tiers.
- `tools(format, selection_len) -> Vec<ToolKey>`: tools per format, with the multi-select filter applied. `tool_tier(tool, format)` and `tools_enabled(format, selection_len, features)` apply tiers.
- `actions() -> &'static [ActionKey; 6]`. Titles come from `ActionKey::title(Platform)`.
- `output_name(source, &Operation, taken: &HashSet<String>) -> String`: the naming rules, with case-insensitive collisions. Page helpers: `produces_pages`, `pages_folder_name`, `page_file_name`.
- `wheel_layout(n) -> Vec<Segment>` and `hit_test(dx, dy, n) -> Option<usize>`: geometry from `INTERACTION_SPEC.md` §4. `GEOMETRY` exposes the constants to the UI, and `clamp_anchor(anchor, work_area)` implements §3.
- `wheel_content(items, mode, features) -> WheelContent`: what each wheel shows, or which empty state.
- `plan(&Selection, &Operation, &Settings) -> Result<JobPlan, PlanError>`: a list of `Job { inputs, output: JobOutput, op, used_fallback }`. The tool's `ToolParams` travel inside `op`, and `JobOutput` is `File`, `Folder` or `PagesFileOrFolder`. The `Selection` carries a `DirSnapshot { names, writable }` per folder the plan may write to, so core does no I/O. **No execution.**
- `Tier` gating: `Features { v2, later }` decides whether v2/later items appear. The full tables stay in the data and are filtered at the edge. See `DECISIONS.md` D12.

### `grape-engines` (executes `JobPlan`s)
- One `Engine` trait per family: `ImageEngine`, `MediaEngine` (FFmpeg), `PdfEngine`, `ArchiveEngine`, `SubtitleEngine`, `DocEngine` (v2).
- Each engine reports progress (0–1, optional) and supports cancellation.
- Engines write to a temp file in the destination folder, then do an atomic rename to the final name. A failed job leaves nothing behind.
- **[Grape]** The final rename must never replace an existing file (no-clobber: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`, `renamex_np(RENAME_EXCL)` on macOS). If the name was taken after planning, re-plan the name. `grape-core`'s collision check can't see every way two names match on disk (e.g. Unicode normalization), so this is the last guard for golden rule 2 (`DECISIONS.md` D12).
- Sidecar binaries are located via a `SidecarPaths` value injected by the app. Never hardcode paths.

### `grape-platform` (OS glue, `#[cfg(target_os)]` modules)
- `DragMonitor` (`start(SyncSender<DragSignal>)`, `stop()`): emits `DragSignal::{Started{modifiers}, Moved{x,y}, Modifiers(Modifiers), Escape, Ended}` on a bounded channel, and drops signals rather than block. `Started` carries the keys already held, which don't open a wheel (`DECISIONS.md` D14). The pure `tracker::DragTracker` holds the drag rules; the Windows hook thread and worker feed it. `drag_monitor()` returns the one for this OS. `mock::ScriptedMonitor` replays scripts for tests.
- `display_bounds_at(x, y) -> Result<DisplayBounds, PlatformError>`: the monitor under the point, as `DisplayBounds { monitor: Rect, work_area: Rect, scale: f64 }` in global coordinates. Those are physical pixels on Windows (`rcMonitor`, `rcWork`). On macOS they're points from the top-left of the primary screen, with `scale` 1.0, read from a screen cache that the main thread refreshes, with a CoreGraphics fallback (D18). The overlay is sized to `monitor`, and the anchor is clamped to `work_area`.
- `windows::install_drop_target(hwnd, sink)`: Grape's own `IDropTarget` on the overlay and its child windows. It sends `DropEvent::{Enter{paths,x,y}, Over{x,y}, Leave, Drop{paths,x,y}}` and never reports a move (`DECISIONS.md` D13).
- `macos::{harden_window, frame_and_show, describe_window}` (main thread only): the overlay's and the toast's NSWindow level, Spaces behavior, and a show that never makes the window key (D18).
- `macos::{show_panel, hide_panel, WindowRole::Panel}` (main thread only): the tool panel at level 99, under the toast (100) and the overlay (101). `show_panel(ns_window, frame) -> PanelShown { front_pid }` records the app in front, sets the frame, `orderFrontRegardless`, `makeKeyWindow`, then asks macOS to activate Grape; `hide_panel(ns_window, back_to_pid) -> bool` orders it out and, if it was key, hands activation back (D31). `macos::mouse_button_down()` (any thread) tells the app whether an Esc the page got belongs to a drag.
- `file_actions`: share, copy, copyPath, move, terminal. Float lives in the app because it's a window. Built on Windows (M5) and macOS (D18): `copy_files(paths)`, `copy_paths(paths)`, `open_terminal(dir, app)` (the `terminal_app` setting), `move_to_chosen_folder(paths) -> MoveOutcome::{Cancelled, AlreadyThere{to}, Moved{to, moved: Vec<MovedItem{from, now}>, unsure, of}}`, `reveal(path)` and `share(paths, near) -> ShareOutcome { opened, skipped: Vec<Skipped { path, why: SkipReason::{Folder, Missing, Unshareable} }> }`. Share is the share sheet on Windows, opened near `near` (global physical pixels; the app passes the wheel's center), with files only (D21). On macOS it's AirDrop with every item, `near` is ignored and nothing is skipped. Each returns `Unsupported` where it isn't built (everything on other OSes). The app's `actions.rs` runs them on a thread of their own and writes the toasts.
- `windows::{harden_shelf, show_shelf, hide_shelf}` and `windows::drag_out(hwnd, paths) -> DragOut { outcome: DragOutcome::{Copied, Linked, Cancelled, ButtonUp, NothingToDrag}, skipped }`, on the main thread, which owns the shelf: the float shelf's window styles, a show that never activates it, and its drag out, which offers copy or link and never move (D21). `windows::primary_button_down()` tells the app's snap timer whether a button is held.
- `windows::{harden_panel, show_panel, hide_panel, other_app_foreground, raise_topmost, escape_down}`, for the tool panel (D31): `harden_panel` gives it the shelf's bits (a tool window that a click activates); `show_panel(hwnd, frame)` shows it topmost without activating it; `hide_panel(hwnd, back_to) -> HandBack::{NotActive, HandedBack, Refused, NoWindow}` hands the foreground back to `back_to` (from `other_app_foreground()`, never one of Grape's windows) if the panel is in front, then hides it (the decision is the pure `win_rules::hand_back_plan`, tested on every OS); `raise_topmost(hwnd)` lifts a window to the top of the topmost band; `escape_down()` reads Esc with `GetAsyncKeyState`, never consuming it. `other_app_foreground` and `escape_down` work on any thread; the rest run on the window's thread.
- `haptic_tick()`: a light trackpad tick on macOS, a no-op elsewhere.
- `file_actions::pick_folder(owner, title, start)`: the native folder picker for Settings, sharing Move's code. It runs on a blocking worker thread (D19).
- See `PLATFORM_MACOS.md` / `PLATFORM_WINDOWS.md`.

### `apps/desktop/src-tauri` (the app)
- Owns the windows: **overlay** (wheel), **settings**, **toast**, the tool **panel**, and on Windows the **float shelf** (`float.rs`, built on first use, D21). The shelf's model is in memory only: items in drop order, no path twice, at most 50. Its page gets ids and names, never paths.
- The **panel** (`panel.rs`, its pure model in `panel/model.rs`, D31) is declared hidden in `tauri.conf.json`: a tool window that a click activates, never focused when shown, one at a time (a new request replaces the open one). A drop on Compress, Audio Channels, Normalize Volume or Merge PDFs opens it only when this build can run the tool: `jobs::check_runnable` is the one check, for the panel route and the direct path. Its page gets a request seq, item ids and display names, never paths, and answers by seq and ids; Rust checks every run, builds the `Operation` with `ToolParams` and the items in the chosen order, and starts the job. It refuses the page's cancel while a mouse button is down (that Esc belongs to a drag), and on Windows the Esc poll acts only until the panel's window first has the focus. Every show and hide is one main-thread reconcile, never Tauri's `show`, `hide` or `set_focus`.
- Runs the `WheelController` state machine and bridges platform signals, UI events and engines.
- Exposes Tauri commands to the UI, each granted only to the window that uses it (`capabilities/`): `wheel_painted` (overlay), `toast_clicked` (toast), the `settings_*` commands (Settings), `float_view`, `float_remove`, `float_clear`, `float_reveal` and `float_drag_out` (the shelf), and `panel_current`, `panel_run` and `panel_cancel` (the panel, `capabilities/panel.json`). `float_drag_out` is async and queues the drag on the main thread; the panel's three are async too.
- Stores settings with `tauri-plugin-store` and handles launch at login with `tauri-plugin-autostart`.

### `apps/desktop/ui` (TypeScript)
- `overlay.ts` renders the wheel SVG from `Segment[]`, highlights from controller events, plays the tick sound.
- `settings.ts` is the settings window.
- `panel.html` and `panel.ts` (with `panel/model.ts`, `panel/copy.ts` and `panel/view.ts`) are the tool panel. It shows what `panel://open` sends and answers with `panel_run` or `panel_cancel` by seq and item ids.
- `float.html` and `float.ts` (with `float/model.ts` and `float/view.ts`) are the shelf. It lists what `float://view` sends, and a press on an item that moves 4 px becomes one `float_drag_out` call with ids. Rust runs the OS drag.
- The UI never decides behavior. It renders what Rust sends and reports pointer/drop info back.

## The overlay window (the hard part)

Requirements:
1. Transparent, borderless, always on top (including over full-screen apps on macOS), with no taskbar or dock entry, and it never steals focus.
2. While the wheel is open it **covers the whole display** under the cursor, so every drop lands in Grape and an off-ring drop can cancel cleanly (`INTERACTION_SPEC.md` §6).
3. It's a drop target that receives file paths and the cursor position during the drag.

Plan:
- One Tauri `WebviewWindow` labelled `overlay`, created hidden at startup with `transparent(true)`, `decorations(false)`, `always_on_top(true)`, `skip_taskbar(true)`, `focused(false)`, `focusable(false)` (on Windows this is what sets `WS_EX_NOACTIVATE`), `shadow(false)` and `visible_on_all_workspaces(true)`. On Windows, `grape_platform::windows::harden_overlay` adds `WS_EX_TOOLWINDOW` and keeps shows from activating the window through a subclass, because tao rewrites the extended style on every show and hide (`DECISIONS.md` D11). `"macOSPrivateApi": true` stays in `tauri.conf.json`; it has been a no-op since Tauri 2.12.1 (the transparency APIs are always on), but tauri-build still requires the matching `macos-private-api` Cargo feature.
- When opened on Windows: `set_position`/`set_size` to the display's frame, then `show()`. On Windows the overlay is then raised to the top of the topmost band on every show (`raise_topmost`), so an open panel or shelf never covers a wheel. On macOS, `grape_platform::macos::frame_and_show` sets the frame in points and calls `orderFrontRegardless` in one main-thread call, which orders the window front although Grape isn't the active app. `focusable: false` keeps it from becoming key. The toast sits one window level below the overlay (D18).
- **Drag events come from the overlay's drop target.** On Windows that's Grape's own `IDropTarget`, sending `grape_platform::DropEvent` in global physical pixels (`DECISIONS.md` D13). On macOS it's wry's own handler, reported as Tauri's `WindowEvent::DragDrop`: wry accepts the drag as a copy, and the app adds the overlay's frame to its window-relative points to make the same `DropEvent`s (D18). The events:
  - `Enter { paths, … }`: **this is where the overlay learns which files are being dragged** on both OSes. Feed `paths` to `grape-core` to build the wheel. No paths means it isn't a file drag: close the wheel.
  - `Over { … }`: hit test → highlight.
  - `Drop { paths, … }`: hit test → run or cancel.
  - `Leave`: the cursor left the display. Keep the wheel and wait. On macOS, a `Leave` while the button is still down with the cursor still on the wheel's display is how Esc shows up (Grape can't read keys without permission), so the controller closes the wheel if no `Enter` or `Over` follows within 200 ms, and a button-up within 50 ms of the `Leave` counts as a refused drop (D18).
- The UI events (`wheel://open|highlight|close`, `toast://show|hide`) and their JSON are specified in `docs/design/m1-drag-wheel.md` §C; both sides test against the same golden fixtures. The shelf has two more, sent only to the `float` window: `float://view` (`{ items: [{ id, name, kind, missing }] }`, after every change and before every show) and `float://drag-ended` (`{ outcome }`: copied, linked, cancelled, nothing or failed), with the fixtures `float-view.json`, `float-view-empty.json` and `float-drag-ended.json` (D21). The panel has two, sent only to the `panel` window: `panel://open` (`{ seq, title, items: [{ id, name }], panel }`, before the window shows; also `panel_current`'s answer) and `panel://close` (`{ seq }`, after the window hid), with 10 fixtures: `panel-open-{compress,channels,normalize,merge}.json`, `panel-close.json`, and the command args `panel-run-{compress,channels,normalize,merge}.json` and `panel-cancel.json` (D31). The wheel opens pending and fills in once `Enter` brings the paths.
- During an OS drag the webview gets **no pointer events**. All highlighting is driven from `Over` positions sent to the UI. Don't rely on DOM `pointermove`.
- **Known risk (spike in M1):** `Enter` may not fire until the cursor moves after the overlay appears. Until it does, the wheel shows a neutral ring (M1: the numbered placeholder ring). macOS works the same way: Grape checks only the drag pasteboard's *types* and never reads its contents (see `PLATFORM_MACOS.md`), so the paths arrive with `Enter` there too.
- macOS full-screen Spaces may need a non-activating `NSPanel`. If a plain Tauri window can't float over full-screen apps, use the `tauri-nspanel` plugin and log it in DECISIONS.

## Threads and timing
- The platform monitor runs on its own thread (macOS: a polling thread; Windows: the hook thread *must* pump messages). It sends signals through a bounded channel.
- The controller runs on the Tauri async runtime. UI updates go out as Tauri events (`wheel://open`, `wheel://highlight`, `wheel://close`).
- Engines run on a worker pool (`tokio::task::spawn_blocking` or a dedicated `rayon` pool). Max concurrency = number of CPUs, and FFmpeg jobs = 2.
- Each file action runs on a "grape-action" thread of its own. On Windows, Share looks the files up there, in the multithreaded apartment. Then "grape-share" opens the sheet from its anchor window, and "grape-action" waits up to 30 s for the sheet to ask for the files. "grape-share" is a single-threaded apartment with a message loop, started on first use and kept for the app's life (D21).
- A drag out of the float shelf runs on the main thread, which owns the shelf window and OLE. It's a modal loop, so Tauri's other main-thread work waits until the drop or the cancel. A "grape-drag-watch" thread watches it and sends Esc after 3 s with no button down. After the user moves the shelf, a "grape-float-snap" thread waits for it to be still and then snaps it on the main thread.
- While a tool panel is open on Windows, "grape-panel-esc" polls Esc every 30 ms and closes the panel on a press made with no mouse button down, until the panel's window first has the focus; after that Esc is the page's (D31). It stops when no panel is open.
- **Budget:** under 16 ms from modifier signal to `wheel://open`, and under 100 ms until the wheel is painted.

## Settings (persisted)

Stored in `settings.json` in the app's data folder by `tauri-plugin-store` (D19). The app's `settings.rs` owns the model, and the Settings window changes it only through the `settings_*` commands.

| Key | Default | Notes |
|---|---|---|
| `keys.conversions` | Shift | `"shift"`, `"ctrl"`, `"alt"` or `"meta"`. The three keys are always different. Windows never offers `meta`. |
| `keys.tools` | Command / Ctrl | |
| `keys.actions` | Control / Alt | |
| `output.location` | `next_to_source` | or an absolute folder path |
| `output.fallback_dir` | `~/Downloads/Grape` | used when the destination isn't writable. Absent means the default. A chosen folder is used only while it exists. |
| `sounds` | on | the highlight tick |
| `haptics` | on (mac) | the trackpad tick, separate from `sounds` |
| `launch_at_login` | on | the user's choice. A release build turns it on at first launch (D19). |
| `terminal_app` | Terminal / Windows Terminal | `"terminal"` or `"iterm2"` on macOS, `"windows_terminal"` or `"powershell"` on Windows |
| `onboarding.done` | absent | `true` once the welcome card's "Got it" is clicked; until then Settings opens at launch (not at login) |
| `tools.compress` | `balanced` | `"small"`, `"balanced"` or `"high"`; the last Compress preset that ran |
| `tools.normalize_lufs` | `-16` | a whole number from −24 to −10; the last Normalize target that ran |
| `features.v2` | off | dev toggle until v2 ships |

## Error model
- Engines return `GrapeError { kind, user_message, detail }`. The toast shows `user_message`, and `detail` goes to the log file.
- Logs go to the app log dir via `tauri-plugin-log`, rotated. **Never log file contents**, only paths and sizes.
