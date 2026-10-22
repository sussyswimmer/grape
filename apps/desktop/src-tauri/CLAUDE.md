# apps/desktop/src-tauri

The Tauri 2 app: windows, tray, `WheelController` state machine, IPC and settings. **Lane A (Claude Code) owns this.**

## Windows (UI windows, not the OS)
| Label | Purpose | Flags |
|---|---|---|
| `overlay` | full-display transparent wheel | transparent, no decorations, always on top, skip taskbar, unfocused, all workspaces, hidden at start |
| `settings` | preferences | normal window, opened from the tray |
| `float` | file shelf | small, topmost, draggable out |
| `toast` | progress and done messages | small, topmost, click → reveal |

The overlay's full spec is in `docs/ARCHITECTURE.md` → "The overlay window". macOS needs `"macOSPrivateApi": true` in `tauri.conf.json`.

## WheelController
- Implements the state machine in `docs/INTERACTION_SPEC.md` §1, exactly. Unit-test it with `grape_platform::mock` signal scripts. It's pure state, so it doesn't need a real OS.
- Inputs: `DragSignal`s, overlay `WindowEvent::DragDrop` (`Enter`/`Over`/`Drop`/`Leave`), Esc.
- Outputs: Tauri events `wheel://open {anchor, mode, segments, labels}`, `wheel://highlight {index|null}`, `wheel://close`, `job://progress`, `job://done`.
- Hit testing uses `grape_core::hit_test` with positions from `DragDropEvent`, converted to logical coordinates relative to the anchor.
- **Watchdog:** if no drag signals or drop events arrive for 30 s while the overlay is visible, hide it.

## Rules
- The app crate is the only place `anyhow` is allowed.
- The settings store is `tauri-plugin-store`. Keep keys exactly as in `docs/ARCHITECTURE.md` → Settings.
- Sidecars go in `bundle.externalBin` with target-triple suffixes. Resolve them through Tauri's sidecar API and pass them to engines as `SidecarPaths`.
- Plugins allowed so far: store, autostart, log, single-instance, shell (sidecars only), updater (opt-in, M7). Adding others needs a DECISIONS entry.
- Mac: `ActivationPolicy::Accessory`. Windows: no main window, tray only.
- **Never request network access** except the updater (golden rule 3).
