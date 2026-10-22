# apps/desktop/src-tauri

The Tauri 2 app: windows, tray, `WheelController` state machine, IPC and settings. **Lane A (Claude Code) owns this.**

## Windows (UI windows, not the OS)
| Label | Purpose | Flags |
|---|---|---|
| `overlay` | full-display transparent wheel | transparent, no decorations, always on top, skip taskbar, unfocused, all workspaces, hidden at start |
| `settings` | preferences | normal window, opened from the tray |
| `float` | file shelf | small, topmost, draggable out |
| `toast` | progress and done messages | small, topmost, click → reveal |

The overlay's full spec is in `docs/ARCHITECTURE.md` → "The overlay window". `"macOSPrivateApi": true` is set in `tauri.conf.json` (a no-op since Tauri 2.12.1, but it must match tauri's `macos-private-api` feature). On Windows the Cargo feature `overlay-subclass` (default on) adds the tool-window and no-activate styles (`docs/DECISIONS.md` D11).

## WheelController
- Implements the state machine in `docs/INTERACTION_SPEC.md` §1, exactly. Unit-test it with `grape_platform::mock` signal scripts. It's pure state, so it doesn't need a real OS.
- Lives in `src/controller.rs` (pure) and `src/wheel.rs` (wiring to the monitor, the drop target and the overlay).
- Inputs: `DragSignal`s (including `Escape`), the overlay's drop events (`grape_platform::DropEvent` from Grape's own drop target on Windows, `docs/DECISIONS.md` D13), Pause, and `wheel_painted` from the overlay.
- Outputs: Tauri events `wheel://open {seq, mode, anchor, segments, geometry}`, `wheel://highlight {index|null}`, `wheel://close`, and later `job://progress`, `job://done`.
- Hit testing uses `grape_core::hit_test` with the drop target's global physical positions, converted to logical offsets from the anchor.
- **Watchdog:** if no drag signals or drop events arrive for 30 s while the overlay is visible, hide it.

## Rules
- The app crate is the only place `anyhow` is allowed.
- The settings store is `tauri-plugin-store`. Keep keys exactly as in `docs/ARCHITECTURE.md` → Settings.
- Sidecars go in `bundle.externalBin` with target-triple suffixes. Resolve them through Tauri's sidecar API and pass them to engines as `SidecarPaths`.
- Plugins allowed so far: store, autostart, log, single-instance, shell (sidecars only), updater (opt-in, M7). Adding others needs a DECISIONS entry.
- Mac: `ActivationPolicy::Accessory`. Windows: no main window, tray only.
- **Never request network access** except the updater (golden rule 3).
