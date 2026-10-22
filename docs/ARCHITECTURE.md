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
- `Format`: enum of all 45 inputs plus target-only formats (`pptx` output, `gif`, `html`, `epub`, …). Includes `from_extension()` and `display_name()`.
- `targets(format) -> Vec<Format>`: the conversion matrix.
- `tools(format, selection_len) -> Vec<ToolKey>`: tools per format, with the multi-select filter applied.
- `actions() -> [ActionKey; 6]`.
- `output_name(source, Operation, existing: &HashSet<String>) -> String`: the naming rules.
- `wheel_layout(n) -> Vec<Segment>` and `hit_test(dx, dy, n) -> Option<usize>`: geometry from `INTERACTION_SPEC.md` §4.
- `plan(selection, Operation, settings) -> JobPlan`: a list of `Job { inputs, output_path, op, params }`. **No execution.**
- `Tier` gating: a `features` struct decides whether v2/later items appear.

### `grape-engines` (executes `JobPlan`s)
- One `Engine` trait per family: `ImageEngine`, `MediaEngine` (FFmpeg), `PdfEngine`, `ArchiveEngine`, `SubtitleEngine`, `DocEngine` (v2).
- Each engine reports progress (0–1, optional) and supports cancellation.
- Engines write to a temp file in the destination folder, then do an atomic rename to the final name. A failed job leaves nothing behind.
- Sidecar binaries are located via a `SidecarPaths` value injected by the app. Never hardcode paths.

### `grape-platform` (OS glue, `#[cfg(target_os)]` modules)
- `DragMonitor`: emits `DragSignal::{Started, Moved{x,y}, Modifiers(set), Ended}` on a channel.
- `display_bounds_at(x, y) -> Rect`: work area of the monitor under the cursor.
- `file_actions`: share, copy, copyPath, move, terminal. Float lives in the app because it's a window.
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
- One Tauri `WebviewWindow` labelled `overlay`, created hidden at startup with `transparent(true)`, `decorations(false)`, `always_on_top(true)`, `skip_taskbar(true)`, `focused(false)`, `shadow(false)` and `visible_on_all_workspaces(true)`. macOS needs `"macOSPrivateApi": true` in `tauri.conf.json` for transparency.
- When opened: `set_position`/`set_size` to the display's frame, then `show()`.
- **Drag events come from Tauri's window event `WindowEvent::DragDrop`:**
  - `Enter { paths, position }`: **this is where the overlay learns which files are being dragged** on both OSes. Feed `paths` to `grape-core` to build the wheel.
  - `Over { position }`: hit test → highlight.
  - `Drop { paths, position }`: hit test → run or cancel.
  - `Leave`: the cursor left the display. Keep the wheel and wait.
- During an OS drag the webview gets **no pointer events**. All highlighting is driven from `Over` positions sent to the UI. Don't rely on DOM `pointermove`.
- **Known risk (spike in M1):** `Enter` may not fire until the cursor moves after the overlay appears. Until it does, the wheel shows a neutral "…" ring. On macOS the drag pasteboard is readable *before* the overlay shows (see `PLATFORM_MACOS.md`), so the wheel can render immediately.
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
