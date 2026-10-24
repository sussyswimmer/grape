# apps/desktop/src-tauri

The Tauri 2 app: windows, tray, `WheelController` state machine, IPC and settings. **Lane A (Claude Code) owns this.**

## Windows (UI windows, not the OS)
| Label | Purpose | Flags |
|---|---|---|
| `overlay` | full-display transparent wheel | transparent, no decorations, always on top, skip taskbar, unfocused, all workspaces, hidden at start |
| `settings` | preferences | normal window, opened from the tray |
| `float` | file shelf | small, topmost, draggable out |
| `toast` | progress and done messages | small, topmost, click → reveal |

The overlay's full spec is in `docs/ARCHITECTURE.md` → "The overlay window". `"macOSPrivateApi": true` is set in `tauri.conf.json` (a no-op since Tauri 2.12.1, but it must match tauri's `macos-private-api` feature). On Windows the Cargo feature `overlay-subclass` (default on) adds the tool-window and no-activate styles (`docs/DECISIONS.md` D11). On macOS the overlay and toast are hardened and shown through `grape_platform::macos` on the main thread (`orderFrontRegardless`, never key), and drops come from wry through `WindowEvent::DragDrop` (D18).

## WheelController
- Implements the state machine in `docs/INTERACTION_SPEC.md` §1, exactly. Unit-test it with `grape_platform::mock` signal scripts. It's pure state, so it doesn't need a real OS.
- Lives in `src/controller.rs` (pure) and `src/wheel.rs` (wiring to the monitor, the drop target and the overlay).
- Inputs: `DragSignal`s (including `Escape`), the overlay's drop events (`grape_platform::DropEvent` from Grape's own drop target on Windows, `docs/DECISIONS.md` D13, or from wry's on macOS, D18), Pause, `Configure` (new keys and sounds from Settings, used from the next modifier; an open wheel isn't redrawn), and `wheel_painted` from the overlay.
- macOS-only rules sit behind `ControllerConfig` flags, `release_fallback` and `leave_ends_drag` (D18). Keep both off on Windows.
- Outputs: the events in `src/events.rs`, which follow `docs/design/m1-drag-wheel.md` §C and are tested against the UI's golden JSON in `ui/src/shared/fixtures/`: `wheel://open {seq, mode, anchor, scale, geometry, keys, sounds, content}`, `wheel://highlight {seq, index|null}`, `wheel://close {seq, reason}`, `toast://show`, `toast://hide`.
- The toast (`src/toast.rs`) sits under the wheel (above it near the bottom of the screen), never takes focus, and hides itself after a few seconds unless it's a "working" toast. Clicks come back as `toast_clicked(id)`; only Rust knows the paths. On Windows it refuses drops; on macOS wry accepts a drop on it as a copy and ignores it, and it sits one window level below the overlay so an open wheel always covers it (D18).
- The wheel opens pending and fills in with `grape_core::wheel_content` when the drop target's Enter brings the paths. A drop on a segment becomes a `RunRequest`: file actions run in `src/actions.rs`, conversions in `src/jobs.rs` (`build_selection` → `plan` → `run_job` on a worker thread, with the toast), and tools say they arrive later.
- Hit testing uses `grape_core::hit_test` with the drop target's global positions, converted to logical offsets from the anchor. Global positions are physical pixels on Windows and points on macOS, where `DisplayBounds.scale` is 1.0. Tauri's `PhysicalPosition` values on macOS are backing pixels, except wry's drop positions (points), so never pass one to the controller unconverted.
- **Watchdog:** if no drag signals or drop events arrive for 30 s while the overlay is visible, hide it.

## Settings
- `src/settings.rs`: the typed `AppSettings`, loaded once in setup (before `wheel::start`) from `settings.json` in the app data folder through `tauri-plugin-store`, and the Settings window's `settings_*` commands. The model (loading, the swap rule, the view, what to store) is pure and tested against the UI's golden `settings-view-*.json`.
- Loading is total: a missing or unusable value (wrong type, unknown key, a key this OS doesn't offer, a relative path) falls back to its default, and keys that aren't three different ones all reset. Warnings name the key, never the value.
- The store ignores a file it can't load and writes over it at exit, so a `settings.json` that's there but can't be read or parsed is renamed to `settings.unreadable.json` (` 2.json`, … when taken; never over a file) before the store opens. If that rename fails there's no store at all. Parse errors are logged by kind and position only.
- A chosen fallback folder is planned with only while it's an existing folder; otherwise the default `Downloads/Grape`. Grape never creates a folder the user chose.
- Every change applies at once and is saved at once: keys and sounds go to the controller (`Input::Configure`), haptics to the flag `wheel.rs` reads on every highlight, folders to the next job (`jobs::submit` plans with `settings::planning_settings`). A failed save is logged and reported, and the change still applies.
- Keys: Windows offers Shift / Ctrl / Alt (never the Windows key), macOS adds Command. Choosing a key another wheel has swaps the two, and the view carries a note saying so. Names come from `Key::name_on(Platform)`.
- The window never sends a path. Folders come from `grape_platform::file_actions::pick_folder` on a blocking worker, owned by the Settings window's HWND on Windows.
- Launch at login is `tauri-plugin-autostart`: a release build turns it on at its first launch only if nothing at all is stored (an unusable value doesn't count), there's a store, and `settings.json` wasn't unreadable at this launch; on macOS also not from `/Volumes/…` or an `AppTranslocation` copy (`settings::first_launch_rule`). Debug builds never touch it on their own. The view shows the plugin's real state.
- The `settings_*` commands are granted only to the `settings` window (`capabilities/settings.json`). No window gets the store's or autostart's own commands.

## Rules
- The app crate is the only place `anyhow` is allowed.
- The settings store is `tauri-plugin-store`. Keep keys exactly as in `docs/ARCHITECTURE.md` → Settings.
- Sidecars go in `bundle.externalBin` with target-triple suffixes. Resolve them through Tauri's sidecar API and pass them to engines as `SidecarPaths`.
- Plugins allowed so far: store, autostart, log, single-instance, shell (sidecars only), updater (opt-in, M7). Adding others needs a DECISIONS entry.
- Mac: `ActivationPolicy::Accessory`. Windows: no main window, tray only.
- **Never request network access** except the updater (golden rule 3).
