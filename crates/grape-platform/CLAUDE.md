# grape-platform

OS glue: drag detection, modifier and cursor state, display bounds, file actions and haptics. **Lane A (Claude Code) owns this crate.** This is the riskiest code in Grape. Read `docs/PLATFORM_MACOS.md` or `docs/PLATFORM_WINDOWS.md` before changing anything here.

**Phase 1 is Windows only** (DECISIONS D8). Until milestone M8, `macos/` is a stub: every function returns `Err(PlatformError::Unsupported)` and `DragMonitor` never emits. It must still compile, so the workspace builds on a Mac. Don't implement AppKit code yet.

## Shape
```
src/
  lib.rs              trait DragMonitor, DragSignal, DropEvent, FileActions, display_bounds_at
  tracker.rs          the pure candidate-drag rules (threshold, baseline keys, Esc, missed release)
  macos/
    drag.rs           drag-pasteboard changeCount + pressedMouseButtons + modifierFlags polling
    actions.rs        AirDrop, pasteboard, NSOpenPanel move, Terminal
    haptics.rs
  windows/
    hook.rs           WH_MOUSE_LL on a message-pumping thread
    drag.rs           the drag worker: allowlist, GetAsyncKeyState polling, DragTracker
    display.rs        display_bounds_at (monitor, work area, DPI scale)
    drop_target.rs    Grape's own IDropTarget for the overlay: always copy, never move (D13)
    overlay.rs        tool-window and no-activate styles for the overlay (D11)
    actions.rs        share sheet, CF_HDROP clipboard, IFileOperation move, wt/PowerShell
  mock.rs             scripted DragSignal sequences for app-level tests
```

## Hard rules
- **Never block the user's input.** Hook callbacks and pasteboard polls do almost nothing: they read state, send it on a bounded channel and return. On Windows, a slow `WH_MOUSE_LL` callback gets the hook silently removed by the OS.
- **No permissions prompts on macOS** (DECISIONS D7). Don't add `NSEvent` *key* monitors or `CGEventTap`, which require Accessibility.
- **Coordinates out of this crate are global, top-left origin, physical pixels.** macOS `mouseLocation` is bottom-left origin in points, so convert here, once.
- Every `unsafe` block gets a `// SAFETY:` comment.
- Gate code with `#[cfg(target_os = "macos")]` / `"windows"`. Both must compile in CI even if you only tested one.

## When you finish a change here
- State which OS you actually ran it on.
- Add or refresh the matching manual QA lines in `docs/TESTING.md`.
- Windows changes near drop handling: re-run "Shift-drop never deletes the source" by hand.
