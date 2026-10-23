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
- `display_bounds_at(x, y) -> Result<DisplayBounds, PlatformError>`: the monitor under the point, as `DisplayBounds { monitor: Rect, work_area: Rect, scale: f64 }` in global physical pixels. The overlay is sized to `monitor` (`rcMonitor`), and the anchor is clamped to `work_area` (`rcWork`). Returns `Unsupported` on macOS until M8.
- `windows::install_drop_target(hwnd, sink)`: Grape's own `IDropTarget` on the overlay and its child windows. It sends `DropEvent::{Enter{paths,x,y}, Over{x,y}, Leave, Drop{paths,x,y}}` and never reports a move (`DECISIONS.md` D13).
- `file_actions`: share, copy, copyPath, move, terminal. Float lives in the app because it's a window. Built so far on Windows (M5): `copy_files(paths)`, `copy_paths(paths)`, `open_terminal(dir)` and `move_to_chosen_folder(paths) -> MoveOutcome::{Cancelled, AlreadyThere{to}, Moved{to, moved: Vec<MovedItem{from, now}>, unsure, of}}`. Each returns `Unsupported` elsewhere. The app's `actions.rs` runs them on a thread of their own and writes the toasts.
- `haptics::tick()` is a no-op on Windows.
- See `PLATFORM_MACOS.md` / `PLATFORM_WINDOWS.md`.

### `apps/desktop/src-tauri` (the app)
- Owns the windows: **overlay** (wheel), **settings**, **float shelf**, **toast**.
- Runs the `WheelController` state machine and bridges platform signals, UI events and engines.
- Exposes Tauri commands to the UI, e.g. `get_wheel(mode)`, `cancel()`, `settings_get/set`.
- Stores settings with `tauri-plugin-store` and handles launch at login with `tauri-plugin-autostart`.

### `apps/desktop/ui` (TypeScript)
- `overlay.ts` renders the wheel SVG from `Segment[]`, highlights from controller events, plays the tick sound.
- `settings.ts` is the settings window.
- `float.ts` is the shelf.
- The UI never decides behavior. It renders what Rust sends and reports pointer/drop info back.

## The overlay window (the hard part)

Requirements:
1. Transparent, borderless, always on top (including over full-screen apps on macOS), with no taskbar or dock entry, and it never steals focus.
2. While the wheel is open it **covers the whole display** under the cursor, so every drop lands in Grape and an off-ring drop can cancel cleanly (`INTERACTION_SPEC.md` §6).
3. It's a drop target that receives file paths and the cursor position during the drag.

Plan:
- One Tauri `WebviewWindow` labelled `overlay`, created hidden at startup with `transparent(true)`, `decorations(false)`, `always_on_top(true)`, `skip_taskbar(true)`, `focused(false)`, `focusable(false)` (on Windows this is what sets `WS_EX_NOACTIVATE`), `shadow(false)` and `visible_on_all_workspaces(true)`. On Windows, `grape_platform::windows::harden_overlay` adds `WS_EX_TOOLWINDOW` and keeps shows from activating the window through a subclass, because tao rewrites the extended style on every show and hide (`DECISIONS.md` D11). `"macOSPrivateApi": true` stays in `tauri.conf.json`; it has been a no-op since Tauri 2.12.1 (the transparency APIs are always on), but tauri-build still requires the matching `macos-private-api` Cargo feature.
- When opened: `set_position`/`set_size` to the display's frame, then `show()`.
- **Drag events come from the overlay's drop target.** On Windows that's Grape's own `IDropTarget`, sending `grape_platform::DropEvent` in global physical pixels (`DECISIONS.md` D13). On macOS it's Tauri's `WindowEvent::DragDrop` unless M8 finds a reason to do the same. The events:
  - `Enter { paths, … }`: **this is where the overlay learns which files are being dragged** on both OSes. Feed `paths` to `grape-core` to build the wheel. No paths means it isn't a file drag: close the wheel.
  - `Over { … }`: hit test → highlight.
  - `Drop { paths, … }`: hit test → run or cancel.
  - `Leave`: the cursor left the display. Keep the wheel and wait.
- The UI events (`wheel://open|highlight|close`, `toast://show|hide`) and their JSON are specified in `docs/design/m1-drag-wheel.md` §C; both sides test against the same golden fixtures. The wheel opens pending and fills in once `Enter` brings the paths.
- During an OS drag the webview gets **no pointer events**. All highlighting is driven from `Over` positions sent to the UI. Don't rely on DOM `pointermove`.
- **Known risk (spike in M1):** `Enter` may not fire until the cursor moves after the overlay appears. Until it does, the wheel shows a neutral ring (M1: the numbered placeholder ring). On macOS the drag pasteboard is readable *before* the overlay shows (see `PLATFORM_MACOS.md`), so the wheel can render immediately.
- macOS full-screen Spaces may need a non-activating `NSPanel`. If a plain Tauri window can't float over full-screen apps, use the `tauri-nspanel` plugin and log it in DECISIONS.

## Threads and timing
- The platform monitor runs on its own thread (macOS: a timer on a dedicated run loop; Windows: the hook thread *must* pump messages). It sends signals through a bounded channel.
- The controller runs on the Tauri async runtime. UI updates go out as Tauri events (`wheel://open`, `wheel://highlight`, `wheel://close`).
- Engines run on a worker pool (`tokio::task::spawn_blocking` or a dedicated `rayon` pool). Max concurrency = number of CPUs, and FFmpeg jobs = 2.
- **Budget:** under 16 ms from modifier signal to `wheel://open`, and under 100 ms until the wheel is painted.

## Settings (persisted)

| Key | Default | Notes |
|---|---|---|
| `keys.conversions` | Shift | |
| `keys.tools` | Command / Ctrl | |
| `keys.actions` | Control / Alt | |
| `output.location` | `next_to_source` | or a fixed folder |
| `output.fallback_dir` | `~/Downloads/Grape` | used when the source folder isn't writable |
| `sounds` | on | |
| `haptics` | on (mac) | |
| `launch_at_login` | on | |
| `terminal_app` | Terminal / Windows Terminal | iTerm2, PowerShell |
| `features.v2` | off | dev toggle until v2 ships |

## Error model
- Engines return `GrapeError { kind, user_message, detail }`. The toast shows `user_message`, and `detail` goes to the log file.
- Logs go to the app log dir via `tauri-plugin-log`, rotated. **Never log file contents**, only paths and sizes.
