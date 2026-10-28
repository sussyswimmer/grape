# grape-platform

OS glue: drag detection, modifier and cursor state, display bounds, file actions and haptics. **Lane A (Claude Code) owns this crate.** This is the riskiest code in Grape. Read `docs/PLATFORM_MACOS.md` or `docs/PLATFORM_WINDOWS.md` before changing anything here.

Windows came first (DECISIONS D8). The macOS port (D18) is real code now: drag polling, display bounds, the overlay window, file actions and haptics. Other hosts (Linux dev machines and CI) get `unsupported.rs`, where everything returns `Err(PlatformError::Unsupported)`.

## Shape
```
src/
  lib.rs              trait DragMonitor, DragSignal, DropEvent, display_bounds_at, haptic_tick
  tracker.rs          the pure candidate-drag rules (threshold or start_now, baseline keys, Esc, missed release)
  file_actions.rs     copy, copy path, terminal (TerminalApp), move, share (ShareOutcome: opened + skipped), reveal, pick_folder (Settings): the API the app calls, per-OS dispatch
  mac_rules.rs        pure macOS rules (Cocoa <-> top-left flips, screen pick, labels, open args, Terminal or iTerm2, volume roots, judge_move, the HEIC worker's decoder budget); tested on every OS
  win_rules.rs        pure Windows rules (Terminal: wt, or PowerShell for a `;` path or by choice; Share: what's skipped, `\\?\` stripped, the sheet's title, the anchor's frame; the tool panel's hand-back plan); tested on every OS
  macos/
    drag.rs           the drag thread: pressedMouseButtons + modifierFlags + drag-pasteboard changeCount/types; starts at once on a file drag session, holds an App Nap activity
    display.rs        screen cache filled on the main thread (refresh_screens), CoreGraphics fallback
    window.rs         overlay/toast/panel NSWindow: level per WindowRole (overlay 101, toast 100, panel 99), collection behavior, frame + orderFrontRegardless; the panel's show_panel (key, activate) and hide_panel (hand back)
    actions.rs        pasteboard copy, open -a Terminal or -b iTerm2 (only if NSWorkspace finds it) / open -R, AirDrop, NSOpenPanel + NSFileManager move (per-item snapshot, volume roots stay, focus handed back), the same panel for pick_folder, haptics
    heif.rs           ImageIo, the HEIC worker's codec (grape_heif_proto::Codec, D33): size and the decoder budget (mac_rules::heif_decode_bytes) from the properties before decoding, `image` after ImageIO's decode and before the bitmap, a full-size upright thumbnail drawn into RGBA8 and un-premultiplied, ICC kept; encode only when CGImageDestinationCopyTypeIdentifiers lists public.heic; CGImageSourceSetAllowableTypes through dlsym (14.2+)
  windows/
    hook.rs           WH_MOUSE_LL on a message-pumping thread
    drag.rs           the drag worker: allowlist, GetAsyncKeyState polling, DragTracker (its key_down also backs mod.rs's escape_down, the tool panel's Esc poll)
    display.rs        display_bounds_at (monitor, work area, DPI scale)
    drop_target.rs    Grape's own IDropTarget for the overlay: always copy, never move (D13); the refusing one for Settings, the toast, the shelf and the tool panel
    overlay.rs        one subclass, two roles: tool-window and no-activate styles for the overlay (D11); tool window that a click activates for the shelf and the tool panel, shown topmost without activation (show_shelf, show_panel) and hidden (hide_shelf; hide_panel hands the foreground back to other_app_foreground's window); raise_topmost for the overlay on every show
    drag_out.rs       drag out of the shelf: SHDoDragDrop of Grape's own data object (CF_HDROP + Preferred DropEffect, never a shell ID list, only what callers stored besides), COPY | LINK never MOVE, button-up guard before and after the path check, Esc watchdog
    share.rs          the share sheet: files looked up as StorageFiles (MTA, 10 s), then the "grape-share" STA thread's invisible anchor window owns the sheet (30 s for DataRequested, Chromium's handler rules)
    actions.rs        CF_HDROP and text clipboard, IFileOpenDialog on an STA thread of its own (Move and pick_folder), IFileOperation move, wt/PowerShell, explorer /select
    job.rs            JobGuard: the Job Object around one helper process (D33): memory cap, kill on close, die on unhandled exception, one active process, UI limits unless the helper is already in a job (then nested, without them: has_ui_limits); peak_memory
  mock.rs             scripted DragSignal sequences for app-level tests
  unsupported.rs      every entry point returns Unsupported (Linux and other hosts)
  bin/
    grape-heif-worker.rs  heif-it only, never shipped: ImageIo behind worker_main, for grape-engines' Mac HEIC tests (GRAPE_TEST_HEIF); "macOS only" elsewhere
tests/
  heif_imageio.rs     macOS + heif-it: ImageIo against the libheif fixtures from CI's Windows job (GRAPE_TEST_HEIF_FIXTURES; fails without it)
  job_object.rs       Windows: the Job Object's cap, kill on close and one-process limit, nested in an outer job and on its own (GRAPE_TEST_JOB_UI=1 outside cargo insists on the UI-limited case); with heif-it, grape-heif.exe under the 2 GiB cap (GRAPE_TEST_HEIF)
```

## Hard rules
- **Never block the user's input.** Hook callbacks and pasteboard polls do almost nothing: they read state, send it on a bounded channel and return. On Windows, a slow `WH_MOUSE_LL` callback gets the hook silently removed by the OS.
- **No permissions prompts on macOS** (DECISIONS D7). Don't add `NSEvent` *key* monitors or `CGEventTap`, which require Accessibility.
- **Coordinates out of this crate are global, top-left origin, y down.** Physical pixels on Windows. Points on macOS, with the origin at the top-left of the primary (menu-bar) screen, and `DisplayBounds::scale` is always 1.0 there. Cocoa (`mouseLocation`, `NSScreen` frames, `setFrame:`) is bottom-left origin, so convert here, once, with the helpers in `mac_rules.rs`.
- **macOS windows.** The overlay is hardened as `WindowRole::Overlay` (level 101) and the toast as `WindowRole::Toast` (100), so an open wheel always covers the toast; never put the toast at or above the overlay's level, or a progress update ends the drag over the wheel. Neither window becomes key because of `"focusable": false` in `tauri.conf.json`. The tool panel is `WindowRole::Panel` (99), under both, and it's the one hardened window that becomes key: `show_panel` makes it key and asks to activate Grape, `hide_panel` hands activation back (D31). Never `NSApp.hide` or tao's `set_focus`.
- **macOS drags.** A file drag session starts the drag at once (`DragTracker::start_now`): AppKit already applied its threshold. The keys held when the poller first sees that session are the baseline, as on Windows. The drag thread holds an `NSProcessInfo` activity for its whole life (App Nap) and waits with `park_timeout`, which `stop` cuts short.
- **macOS Move.** A mounted volume's root (its own `lstat` device isn't its folder's) never moves and isn't counted as moved or unsure. Each item is judged (`mac_rules::judge_move`) from a snapshot taken right after its own move, before the next item moves. Grape activates itself for the panel and alert, then hands activation back to the app that was in front; never `NSApp.hide`. Share does the same through an AirDrop delegate (`ShareDone`), kept alive with its service until AirDrop reports back, and only once no AirDrop window is key. Settings' `pick_folder` shares Move's panel but neither activates nor hands back: the Settings window is already key.
- **macOS threads.** `NSScreen`, `NSWindow`, `NSOpenPanel` and `NSAlert` are main-thread only (`MainThreadMarker`). Main-thread UI from a worker goes through `dispatch2::run_on_main`, which waits: never call it from a thread the main thread is waiting for. Tauri's `ns_window()` pointer is only good on the main thread until the autorelease pool drains, so retain it at once. Wrap AppKit work on Grape's own threads in `autoreleasepool`.
- **The HEIC worker (D33).** `macos::heif` runs only in the worker process (`Grape --grape-heif-worker`, or the test bin), never in Grape's own: a crash in ImageIO must end one job, not the tray and the drag. `CGImageSourceSetAllowableTypes` restricts ImageIO for the whole process, so it's called only there, first thing and on one thread (`ImageIo::restrict_process`, from `main.rs` and the test bin; never from the codec's calls, which tests run on many threads), and only through `dlsym`: never the objc2-image-io binding, whose import would stop Grape from launching on macOS 13 (CI's DMG job checks `nm -um`). No failure detail holds a path, and the codec logs nothing.
- Every `unsafe` block gets a `// SAFETY:` comment.
- **Logging** (`log`) is only for saying why an action did something other than what was asked (Terminal opening another app than the one picked), and for the Windows share sheet's HRESULTs and the drag-out watchdog, which only a run on a real PC can check. Never log a path or a file name.
- Gate code with `#[cfg(target_os = "macos")]` / `"windows"`. Both must compile in CI even if you only tested one. From Linux, check the Mac target with `CC_aarch64_apple_darwin=clang AR_aarch64_apple_darwin=llvm-ar-18 CFLAGS_aarch64_apple_darwin="--target=arm64-apple-macos11" cargo clippy -p grape-platform --all-targets --target aarch64-apple-darwin -- -D warnings`.
- Tests never open a panel, an alert, AirDrop or the share sheet, never start a real drag, and never call `run_on_main` (nothing serves the main queue under `cargo test`). On Windows a test may only reach code that returns before `ShowShareUIForWindow` or `SHDoDragDrop`: nothing left to share, or the button already up.

## When you finish a change here
- State which OS you actually ran it on.
- Add or refresh the matching manual QA lines in `docs/TESTING.md`.
- Windows changes near drop handling: re-run "Shift-drop never deletes the source" by hand.
