# Windows platform notes

Target: Windows 10 22H2+ and Windows 11, x64 and ARM64. WebView2 is preinstalled on 11. On 10, Tauri's bootstrapper installs it. Rust bindings: the `windows` crate.

## Dev setup (on the Windows PC)

Install these once, in PowerShell:

```powershell
winget install --id Git.Git -e
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install --id Rustlang.Rustup -e
winget install --id OpenJS.NodeJS.LTS -e
```

Then open a **new** terminal and run:

```powershell
rustup default stable-msvc
rustup target add aarch64-pc-windows-msvc   # only if building ARM64 too
npm install -g pnpm@10
```

- WebView2 is built into Windows 11. On Windows 10, install the Evergreen runtime if `pnpm tauri dev` asks for it.
- Check the setup with `rustc -vV` (host should end in `windows-msvc`), `node -v` and `pnpm -v`.
- Keep the repo on a local drive, **not** inside OneDrive. OneDrive syncing `target/` and `node_modules/` makes builds slow and flaky.
- The first `cargo build` takes several minutes, because Tauri has many dependencies. Later builds are incremental.
- Windows Defender can slow builds a lot. Optionally add the repo folder as an exclusion in Windows Security → Virus & threat protection → Exclusions.

## Detecting a file drag

Windows has no system-wide "drag in progress" signal, and the dragged data object is only visible to a window the cursor enters. So detection is a **heuristic**, and the overlay is what learns the file list.

1. **Low-level mouse hook** (`SetWindowsHookExW(WH_MOUSE_LL)`) on a dedicated thread **that pumps messages** (`GetMessageW` loop). A hook thread that doesn't pump freezes the whole system's mouse.
   - `WM_LBUTTONDOWN` records the start point.
   - `WM_MOUSEMOVE` with the button down past `SM_CXDRAG`/`SM_CYDRAG` → candidate drag.
   - `WM_LBUTTONUP` → `Ended`.
   - Keep the hook callback tiny: post to a channel and return `CallNextHookEx` immediately. Windows silently removes hooks that exceed `LowLevelHooksTimeout`.
2. **Modifiers:** `GetAsyncKeyState(VK_SHIFT / VK_CONTROL / VK_MENU)` polled at ~120 Hz while a candidate drag is live. This avoids a keyboard hook entirely. Add `WH_KEYBOARD_LL` only if polling misses fast taps in testing.
3. **Is it a *file* drag?** Unknown until the overlay gets `DragDropEvent::Enter { paths }`.
   - **[Grape]** Reduce false positives: only arm when the window under the cursor at mouse-down (`WindowFromPoint` → `GetAncestor(GA_ROOT)` → class name) is Explorer (`CabinetWClass`, `ExploreWClass`) or the desktop (`Progman`, `WorkerW`), **or** the user enabled "Any app" in Settings.
   - If the overlay shows but `Enter` brings no paths within 300 ms of the next cursor movement, hide it.
4. **Tap trigger:** in Explorer, holding Shift while dropping switches the drag to *move* and Ctrl to *copy*. Grape opens on the key going **down**. The drop then happens on Grape's overlay, so Explorer's effect choice no longer matters (see the next section).

## The drop must never move the source

Explorer's source-side behaviour depends on the drop effect the target reports. With Shift held, the user's intent looks like "move". If a target returns `DROPEFFECT_MOVE` and doesn't handle the move itself, Explorer can **delete the source** ("optimized move" / `CFSTR_PERFORMEDDROPEFFECT`).

- Tauri/wry's drop handler is expected to return `DROPEFFECT_COPY`. **Verify this in M1** with a test that Shift-drags a file onto the overlay and asserts the source still exists. If wry returns anything else, register our own `IDropTarget` on the overlay HWND and always return `DROPEFFECT_COPY`. Log the choice in DECISIONS.
- This is golden rule 2, and it's the single most important Windows test.

## Overlay window
- Tauri window as described in `ARCHITECTURE.md`, plus `WS_EX_TOOLWINDOW` (no taskbar or Alt-Tab), `WS_EX_NOACTIVATE`, and `HWND_TOPMOST`.
- Size it to the monitor under the cursor using `MonitorFromPoint` + `GetMonitorInfoW` (`rcWork` for clamping, `rcMonitor` for size).
- Per-monitor DPI v2: Tauri already declares it. Convert hook coordinates (physical) consistently and test mixed 100%/150% setups.
- **UIPI:** drags from an *elevated* app (e.g. an admin Explorer window) can't drop onto a non-elevated Grape. That's expected; document it and don't try to run Grape elevated.

## Sound
No haptics on Windows. The tick plays via WebAudio in the overlay or `PlaySoundW` with `SND_ASYNC | SND_MEMORY` on a preloaded WAV.

## File actions
- Share: `IDataTransferManagerInterop::ShowShareUIForWindow` from the float/toast window. Populate `DataPackage.SetStorageItems` with the files. Nearby Share appears if the user enabled it.
- Copy: `OleSetClipboard` with a data object carrying `CF_HDROP` + `Preferred DropEffect = DROPEFFECT_COPY`.
- Move: `IFileOpenDialog` with `FOS_PICKFOLDERS`, then `IFileOperation::MoveItem` with `FOF_ALLOWUNDO` so Ctrl+Z in Explorer undoes it.
- Copy Path: `CF_UNICODETEXT`, paths joined with `\r\n`.
- Terminal: `wt.exe -d "<dir>"` if `wt` resolves; otherwise `powershell.exe -NoExit -Command "Set-Location -LiteralPath '<dir>'"`.

## Conversions that use Windows components
- **HEIC decode:** bundled libheif + libde265 (LGPL, as DLLs) so it works without the Store HEIF extension.
- **HEIC encode:** bundled `heif-enc` sidecar built with x265. That makes the sidecar GPL, which is fine because Grape is free and open source and it's a separate executable (`ENGINES.md` licensing rules).
- **H.264 encode:** the FFmpeg sidecar with libx264, allowed under the same rules. Prefer `h264_mf` / `h264_nvenc` / `h264_qsv` / `h264_amf` hardware encoders when present, with x264 as the fallback.

## Distribution
- Installer: Tauri **NSIS** bundle (per-user install, no admin needed) plus a portable `.zip`. Keep MSI for later.
- Code signing: unsigned builds trigger SmartScreen ("Windows protected your PC"). Free option for open source: **SignPath Foundation** (free signing for OSS projects that meet its criteria). Alternatives: Azure Trusted Signing (paid, identity checks). Log in DECISIONS when chosen.
- Launch at login: `tauri-plugin-autostart` (HKCU Run key).
- Builds: GitHub Actions `windows-latest` (x64) and `windows-11-arm` (ARM64) runners. See ROADMAP M7.

## Windows QA must-checks
- Drag from Explorer, the Desktop, Downloads, search results, OneDrive placeholders (cloud-only files: Grape must hydrate or skip them, not hang), and zip folders opened in Explorer (virtual paths, which v1 should skip).
- Shift-drop never deletes the source.
- Explorer running elevated vs not.
- Two monitors at different DPI.
- Tablet/touch: Windows touch drags don't use the mouse hook path. Unsupported in v1, so make sure nothing breaks.
