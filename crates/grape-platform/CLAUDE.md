# grape-platform

OS glue: drag detection, modifier and cursor state, display bounds, file actions and haptics. **Lane A (Claude Code) owns this crate.** This is the riskiest code in Grape. Read `docs/PLATFORM_MACOS.md` or `docs/PLATFORM_WINDOWS.md` before changing anything here.

Windows came first (DECISIONS D8). The macOS port (D18) is real code now: drag polling, display bounds, the overlay window, file actions and haptics. Other hosts (Linux dev machines and CI) get `unsupported.rs`, where everything returns `Err(PlatformError::Unsupported)`.

## Shape
```
src/
  lib.rs              trait DragMonitor, DragSignal, DropEvent, display_bounds_at, haptic_tick
  tracker.rs          the pure candidate-drag rules (threshold or start_now, baseline keys, Esc, missed release)
  file_actions.rs     copy, copy path, terminal, move, share, reveal: the API the app calls, per-OS dispatch
  mac_rules.rs        pure macOS rules (Cocoa <-> top-left flips, screen pick, labels, volume roots, judge_move); tested on every OS
  macos/
    drag.rs           the drag thread: pressedMouseButtons + modifierFlags + drag-pasteboard changeCount/types; starts at once on a file drag session, holds an App Nap activity
    display.rs        screen cache filled on the main thread (refresh_screens), CoreGraphics fallback
    window.rs         overlay/toast NSWindow: level per WindowRole (overlay 101, toast 100), collection behavior, frame + orderFrontRegardless
    actions.rs        pasteboard copy, open -a Terminal / open -R, AirDrop, NSOpenPanel + NSFileManager move (per-item snapshot, volume roots stay, focus handed back), haptics
  windows/
    hook.rs           WH_MOUSE_LL on a message-pumping thread
    drag.rs           the drag worker: allowlist, GetAsyncKeyState polling, DragTracker
    display.rs        display_bounds_at (monitor, work area, DPI scale)
    drop_target.rs    Grape's own IDropTarget for the overlay: always copy, never move (D13)
    overlay.rs        tool-window and no-activate styles for the overlay (D11)
    actions.rs        CF_HDROP and text clipboard, IFileOperation move, wt/PowerShell, explorer /select (share later)
  mock.rs             scripted DragSignal sequences for app-level tests
  unsupported.rs      every entry point returns Unsupported (Linux and other hosts)
```

## Hard rules
- **Never block the user's input.** Hook callbacks and pasteboard polls do almost nothing: they read state, send it on a bounded channel and return. On Windows, a slow `WH_MOUSE_LL` callback gets the hook silently removed by the OS.
- **No permissions prompts on macOS** (DECISIONS D7). Don't add `NSEvent` *key* monitors or `CGEventTap`, which require Accessibility.
- **Coordinates out of this crate are global, top-left origin, y down.** Physical pixels on Windows. Points on macOS, with the origin at the top-left of the primary (menu-bar) screen, and `DisplayBounds::scale` is always 1.0 there. Cocoa (`mouseLocation`, `NSScreen` frames, `setFrame:`) is bottom-left origin, so convert here, once, with the helpers in `mac_rules.rs`.
- **macOS windows.** The overlay is hardened as `WindowRole::Overlay` (level 101) and the toast as `WindowRole::Toast` (100), so an open wheel always covers the toast; never put the toast at or above the overlay's level, or a progress update ends the drag over the wheel. Neither window becomes key because of `"focusable": false` in `tauri.conf.json`.
- **macOS drags.** A file drag session starts the drag at once (`DragTracker::start_now`): AppKit already applied its threshold. The keys held when the poller first sees that session are the baseline, as on Windows. The drag thread holds an `NSProcessInfo` activity for its whole life (App Nap) and waits with `park_timeout`, which `stop` cuts short.
- **macOS Move.** A mounted volume's root (its own `lstat` device isn't its folder's) never moves and isn't counted as moved or unsure. Each item is judged (`mac_rules::judge_move`) from a snapshot taken right after its own move, before the next item moves. Grape activates itself for the panel and alert, then hands activation back to the app that was in front; never `NSApp.hide`. Share does the same through an AirDrop delegate (`ShareDone`), kept alive with its service until the next share.
- **macOS threads.** `NSScreen`, `NSWindow`, `NSOpenPanel` and `NSAlert` are main-thread only (`MainThreadMarker`). Main-thread UI from a worker goes through `dispatch2::run_on_main`, which waits: never call it from a thread the main thread is waiting for. Tauri's `ns_window()` pointer is only good on the main thread until the autorelease pool drains, so retain it at once. Wrap AppKit work on Grape's own threads in `autoreleasepool`.
- Every `unsafe` block gets a `// SAFETY:` comment.
- Gate code with `#[cfg(target_os = "macos")]` / `"windows"`. Both must compile in CI even if you only tested one. From Linux, check the Mac target with `CC_aarch64_apple_darwin=clang AR_aarch64_apple_darwin=llvm-ar-18 CFLAGS_aarch64_apple_darwin="--target=arm64-apple-macos11" cargo clippy -p grape-platform --all-targets --target aarch64-apple-darwin -- -D warnings`.
- Tests never open a panel, an alert or AirDrop, and never call `run_on_main` (nothing serves the main queue under `cargo test`).

## When you finish a change here
- State which OS you actually ran it on.
- Add or refresh the matching manual QA lines in `docs/TESTING.md`.
- Windows changes near drop handling: re-run "Shift-drop never deletes the source" by hand.
