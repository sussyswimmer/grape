# M1 + first M3 slice: working design note

> Working note, not spec. It records what was verified in the Tauri 2.12.1 / wry 0.57.0 / tao 0.37.1 / windows 0.62.2 sources (paths are relative to the cargo registry) and the contracts the drag pipeline, wheel UI and engines are built against. If it disagrees with `docs/*.md`, the spec docs win; decisions that stick get a `DECISIONS.md` entry. Written 2026-10-22.


Repo: `/home/user/grape`, branch `claude/vigilant-gauss-l5qt5y`, base `a6d1bf5`. Target: a Windows build Maxwell can test. He drags a file in Explorer or on the Desktop and taps Shift, Ctrl or Alt. The real wheel for that file appears at the cursor. Hovering highlights a segment, shows a center label and plays a tick. Esc closes the wheel. Dropping off the ring cancels and swallows the drop. Dropping on a segment runs it. In this slice only raster image conversions among jpg/png/webp/tiff/bmp actually execute. A toast shows "Making {name}…" and then "Done" or the error.

Sections 0 and 1 are for everyone. Sections A to I are addressed to each agent. Section J covers risks and QA.

---

## 0. Blocking discoveries (read first)

1. **wry's drop target never sets the drop effect in `Drop`. Grape must register its own `IDropTarget` on every Grape window (golden rule 2).** Evidence is in §1.1. `DragEnter` and `DragOver` return `DROPEFFECT_COPY` for file drags, but `Drop` leaves `*pdwEffect` untouched. OLE sets `*pdwEffect = dwOKEffect` (COPY|MOVE|LINK for Explorer) right before calling `Drop`, so the effect reported back to the source can include MOVE. Whether Explorer then deletes the source is unknowable from here, so we remove the doubt: our own target always writes `DROPEFFECT_COPY`. tao registers the same flawed handler on the top-level HWND. The settings window and the new toast window are affected too. Dropping a file on Settings today could, in principle, report MOVE.
2. **On Windows, Esc during an OLE drag cancels the drag at the source.** Explorer's `IDropSource::QueryContinueDrag` returns `DRAGDROP_S_CANCEL` when `fEscapePressed`. The overlay never takes focus, so Esc reaches Explorer's drag loop. "Esc hides the wheel and the OS drag continues to a normal drop" (INTERACTION_SPEC §6, ROADMAP M1, TESTING) therefore can't hold on Windows: Esc closes the wheel *and* the OS cancels the drag. That is exactly what happens without Grape, so golden rule 4 holds. The spec and QA text must change (D14). This is expected from documented OLE behavior and must be confirmed on the PC.
3. **Ended races Drop.** The low-level hook sees the button-up *before* Explorer's OLE loop calls our `Drop` (cross-process COM). If the controller hid the overlay on `Ended`, the drop could fall through to the window below, maybe with Shift held, which means a *move*. The controller therefore keeps the overlay up for a grace window after `Ended` whenever the drag is inside the overlay (§B).
4. **Shift held *before* a drag must not open the wheel.** Shift-drag is Explorer's "move" gesture. The wheel opens only on a key-down edge during the drag, and modifiers held at drag start form the baseline (§B, D22).
5. **The UI has no `@tauri-apps/api`**, so it can't listen for events yet. Add `@tauri-apps/api@2.12.1` (2026-09-30, Apache-2.0 OR MIT), matching tauri 2.12.1 (D20).
6. **Tick sound:** WebView2 may block an `AudioContext` that has had no user gesture, and the overlay never gets one. Tauri has no autoplay option, but wry appends `--autoplay-policy=no-user-gesture-required` only through `additionalBrowserArgs`. Every window in one WebView2 data directory must use **identical** args (tauri-utils `config.rs:2279`), so set one constant string on overlay, toast and settings (D20).

---

## 1. Verified facts (evidence from `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`)

### 1.1 Windows drag-drop in wry 0.57.0 / tao 0.37.1
- wry uses **its own** `IDropTarget` and turns off WebView2's drop handling: `wry-0.57.0/src/webview2/mod.rs:156-164`. When a drag-drop handler exists, it calls `SetAllowExternalDrop(false)` and then `DragDropController::new(hwnd, handler)`.
- Registration: `drag_drop.rs:50-76`. It walks `EnumChildWindows(container)`, and for each child calls `RevokeDragDrop` and then `RegisterDragDrop(hwnd, target)`. Windows of other processes fail registration and are skipped.
- `DragEnter` (`drag_drop.rs:159-199`) sets `*pdwEffect = DROPEFFECT_COPY` only when `CF_HDROP` is present. For a drag without files it returns early at `:174-176` **without writing `*pdwEffect`**. It never looks at `grfKeyState`, so Shift doesn't change the effect.
- `DragOver` (`:201-217`) writes the cached `cursor_effect`: COPY for a file drag. It is never MOVE.
- **`Drop` (`:226-250`) never writes `_pdwEffect`.** It also frees the `GetData` medium with `DragFinish` instead of `ReleaseStgMedium`.
- tao registers a second handler on the **top-level** HWND: `tao-0.37.1/src/platform_impl/windows/window.rs:1137-1166`, with `drag_and_drop` defaulting to `true` at `platform_impl/windows/mod.rs:53`. Its `Drop` also leaves the effect alone (`drop_handler.rs:159-175`).
- What OLE puts in `*pdwEffect` for `Drop`: in Wine's reimplementation (`dlls/ole32/ole2.c`, `drag_end`, lines 2219-2227 of current master), OLE sets `*info->pdwEffect = info->dwOKEffect;`, calls `IDropTarget_Drop(...)`, then does `*info->pdwEffect &= info->dwOKEffect;`. Microsoft's docs for `IDropTarget::Drop` say the same: "On input, pointer to the value of the pdwEffect parameter of DoDragDrop." So a target that doesn't write the value returns the source's full allowed set. **Conclusion:** wry isn't "COPY always". We need our own target (D13).
- OLE finds the target by walking up parents from the window under the cursor (`drag_enter`, ole2.c:2189-2190: `while (new_target && !is_droptarget(new_target)) new_target = GetParent(new_target);`). The drag tracker re-evaluates on a **50 ms timer** as well as on mouse moves (ole2.c:2152 `SetTimer(..., 50, ...)`, :2156-2167). So `DragEnter` and `DragOver` arrive without cursor motion, about every 50 ms in Wine. Windows' own timer cadence isn't verified; the spike logs it.
- `Drop` is only called when the last effect wasn't `DROPEFFECT_NONE` (ole2.c:2221-2222); otherwise OLE calls `DragLeave`. So a target that answers NONE never gets a drop. That is how the toast and settings windows refuse drops.

### 1.2 Tauri 2.12.1 `DragDropEvent` (for reference; our own target replaces it)
- Type: `tauri-runtime-2.12.1/src/window.rs:99-121`, with `Enter{paths, position: PhysicalPosition<f64>}`, `Over{position}`, `Drop{paths, position}` and `Leave`.
- Coordinates are **physical pixels relative to the client area of the HWND that received the drop**: wry calls `ScreenToClient(self.hwnd, &mut pt)` (`drag_drop.rs:166-167, 208-209, 234-235`), and `tauri-runtime-wry-2.12.1/src/lib.rs:4783-4815` wraps them in `PhysicalPosition` unchanged.
- Delivery: the event goes to the window as a `SynthesizedWindowEvent::DragDrop` (`lib.rs:4809-4811`) and becomes `WindowEvent::DragDrop` (`:1419`). You'd listen with `WebviewWindow::on_window_event` (`tauri-2.12.1/src/webview/webview_window.rs:1643`). `Over` fires on every OLE `DragOver`, which means continuously (§1.1).
- **Decision:** we don't use these events. Our target reports **global screen physical** coordinates (the `POINTL` that OLE hands `IDropTarget` is in screen coordinates), the same space as the mouse hook. That removes a coordinate conversion.

### 1.3 Transparent overlay hit-testing (tao 0.37.1)
- A transparent window is **not** `WS_EX_LAYERED`. `on_create` calls `DwmEnableBlurBehindWindow` with an empty region when `transparent && !no_redirection_bitmap` (`window.rs:1203-1216`). `WS_EX_NOREDIRECTIONBITMAP` is set only when `no_redirection_bitmap` is on (`window_state.rs:261-263`), Tauri passes `config.no_redirection_bitmap` (`tauri-runtime-wry/src/lib.rs:868`), and our config leaves it false.
- `WS_EX_TRANSPARENT | WS_EX_LAYERED` is added **only** by `set_ignore_cursor_events(true)` (`window_state.rs:282-284`, `window.rs:530-540`). That would make `WindowFromPoint`, and with it OLE, skip the overlay, so drops would pass through. **Never call `set_ignore_cursor_events(true)` on the overlay or toast.**
- A non-layered window is hit-tested by its rectangle, not its pixels. Drops on fully transparent areas therefore land on the overlay: the WebView2 child or our container, then up the parent chain to our registered target. **Belt and braces:** the overlay body gets a near-invisible fill, `--overlay-hit-fill: rgba(0, 0, 0, 0.004)` (one level of 255), in case WebView2's child does per-pixel `HTTRANSPARENT`. It's a QA item (§J) and can be set to `transparent` if the PC proves it isn't needed (D15).
- Moving between monitors at different DPI: tao handles `WM_DPICHANGED` by **resizing** the window (`event_loop.rs:1837-1990`). So place the overlay with `set_position`, then `set_size`, then `set_position` again, all inside one `run_on_main_thread` closure, and check the result (§F).
- Order: config windows are built **before** the user's `setup` runs (`tauri-2.12.1/src/app.rs:2691-2698`), and wry's `new_in_hwnd` waits for the controller synchronously. So in `setup`, the overlay's WebView2 child HWNDs and wry's registrations already exist, and we can replace them.

### 1.4 Tauri APIs (2.12.1, `src/webview/webview_window.rs` unless noted)
| API | Where | Notes |
|---|---|---|
| `run_on_main_thread(f)` | :1633 | Use it for every overlay placement/show/hide and for drop-target installs |
| `on_window_event(f)` | :1643 | Not needed for drops any more |
| `monitor_from_point(x, y)` / `available_monitors()` | :1943 / :1948 | `Monitor{position, size, work_area, scale_factor}` (`window/mod.rs:58-101`). Use only as a cross-check; `display_bounds_at` is the source of truth |
| `hwnd()` | :1966 | Windows only, for platform calls |
| `set_ignore_cursor_events` | :2260 | **Don't use it** (§1.3) |
| `show()` / `hide()` | :2334 / :2339 | Our subclass adds `SWP_NOACTIVATE` (D11). Never call `set_focus` on overlay or toast |
| `set_size(PhysicalSize)` / `set_position(PhysicalPosition)` | :2359 / :2382 | Physical, global |
| `Emitter::emit_to(target, event, payload)` | `lib.rs:1009` | `app.emit_to("overlay", "wheel://open", payload)`. `S: Serialize + Clone`. Callable from any thread |
| `tauri::async_runtime::spawn_blocking` | `async_runtime.rs` | Job runner |
| `app.path().download_dir()` | `path` | Fallback dir `…\Downloads\Grape`, with no new dependency |
| `additionalBrowserArgs` | tauri-utils `config.rs:2133` | Every window must use the same string (`:2279`). wry's default is `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection` (`wry webview2/mod.rs:300-307`) |
| `backgroundThrottling` | `config.rs:2250` | "Windows: unsupported", so don't bother |

### 1.5 Win32 with `windows` 0.62.2
- `SetWindowsHookExW(WH_MOUSE_LL, Some(proc), Some(GetModuleHandleW(None)?.into()), 0)` on a dedicated thread running a `GetMessageW` loop. Call `PeekMessageW(PM_NOREMOVE)` once first so the thread queue exists before anyone `PostThreadMessageW`s it. To stop: `PostThreadMessageW(tid, WM_QUIT, …)`, the loop exits, then `UnhookWindowsHookEx` on that thread, then join.
- `MSLLHOOKSTRUCT { pt, mouseData, flags, time, dwExtraInfo }` (`UI/WindowsAndMessaging/mod.rs:5193`). `pt` is in per-monitor-aware screen coordinates, which is physical for our PMv2 process.
- `LowLevelHooksTimeout` (HKCU\Control Panel\Desktop; about 300 ms by default, 1000 ms at most on Win7+): Windows silently unhooks a callback that's too slow. The callback only `try_send`s and returns `CallNextHookEx`. Raise the hook thread to `THREAD_PRIORITY_HIGHEST`.
- Callback handoff: a `static HOOK_TX: Mutex<Option<SyncSender<RawMouse>>>`. In the callback use `try_lock()` then `try_send`; if either fails, drop the event and never block. It's set before `SetWindowsHookExW` and cleared after unhooking, which allows restarts (OnceLock can't be reset).
- `GetAsyncKeyState(VK_SHIFT | VK_CONTROL | VK_MENU | VK_LWIN/VK_RWIN | VK_ESCAPE | VK_LBUTTON)` (`UI/Input/KeyboardAndMouse/mod.rs:28`); held means `< 0`. Mouse buttons are **physical**, so when `GetSystemMetrics(SM_SWAPBUTTON) != 0`, poll `VK_RBUTTON` for the logical left button.
- `GetSystemMetrics(SM_CXDRAG / SM_CYDRAG)`: a drag means `|dx| > cx || |dy| > cy` from the down point.
- Allowlist: `WindowFromPoint(pt)`, then `GetAncestor(hwnd, GA_ROOT)`, then `GetClassNameW`. Explorer windows, including Windows 11 tabs, Home, Gallery, Downloads and search results, are top-level `CabinetWClass` (legacy `ExploreWClass`). Desktop icons are `SysListView32` inside `SHELLDLL_DefView`, under `Progman` or under a `WorkerW` (slideshow / Win+Tab reparenting). On 24H2 the `WorkerW` is a child of `Progman`. `GA_ROOT` reaches the right top-level in every case. Default allowlist: `CabinetWClass, ExploreWClass, Progman, WorkerW`. Our own process's windows are never armed (`GetWindowThreadProcessId` pid == `GetCurrentProcessId`).
- `display_bounds_at`: `MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)` (`Graphics/Gdi/mod.rs:1474`), then `GetMonitorInfoW` (`:1102`) for `rcMonitor` and `rcWork`, then `GetDpiForMonitor(h, MDT_EFFECTIVE_DPI)` (`UI/HiDpi/mod.rs:37-39`, gated on `Win32_Graphics_Gdi`). `scale = dpi_x / 96`.
- Drop target: `#[implement(IDropTarget)]` (`windows_core::implement`, always re-exported). `IDropTarget_Impl` needs `Win32_System_Com` and `Win32_System_SystemServices` (`System/Ole/mod.rs:6104`). `ReleaseStgMedium` needs `Win32_Graphics_Gdi` and `Win32_System_Com_StructuredStorage` (`:1012-1014`). `DragQueryFileW` is in `Win32_UI_Shell`. `OleInitialize` is already done on the main thread by tao (`window.rs:1140`).
- **Features to add to grape-platform's `windows` dependency:** `Win32_Graphics_Gdi`, `Win32_System_Com`, `Win32_System_Com_StructuredStorage`, `Win32_System_LibraryLoader`, `Win32_System_Memory` (`GlobalAlloc`/`GlobalLock` for the test data object), `Win32_System_Ole`, `Win32_System_SystemServices`, `Win32_UI_HiDpi`, `Win32_UI_Input_KeyboardAndMouse`. These come on top of the existing `Win32_Foundation`, `Win32_System_Threading`, `Win32_UI_Shell` and `Win32_UI_WindowsAndMessaging`. Features don't change `Cargo.lock`.

### 1.6 `image` crate and no-clobber finalize
- **image 0.25.10** is the newest release (2026-03-10, 226 days old). MIT OR Apache-2.0, MSRV 1.88 (we use 1.99). Default features are `rayon` plus every format. Use `default-features = false, features = ["jpeg", "png", "webp", "bmp", "tiff"]`.
- Required transitive dependencies: bytemuck (Zlib/Apache/MIT), byteorder-lite (Unlicense/MIT), **moxcms ^0.8 → 0.8.1** (2026-03-07, BSD-3-Clause OR Apache-2.0, pulls pxfm under BSD-3 OR Apache) and num-traits. Format dependencies: zune-jpeg 0.5.x and zune-core (MIT/Apache/Zlib), png 0.18.1 (already in the lock), image-webp 0.2.4, and tiff 0.11.x (**MIT**) with weezl, fdeflate, flate2 and half. **No GPL, LGPL or MPL** anywhere in this tree.
- 14-day rule: `crc32fast 1.5.3` (2026-10-20) is too young, and the lock keeps 1.5.2. Don't run `cargo update` broadly. After `cargo add`, check every *new* `Cargo.lock` entry's publish date (script in §E).
- WebP encoding is **lossless only**: `WebPEncoder::new_lossless` (`src/codecs/webp/encoder.rs:34`, which takes Rgb8/Rgba8/L8/La8). Lossy WebP would need libwebp, so record that as a known limitation.
- JPEG quality: `JpegEncoder::new_with_quality(w, 90)` (`jpeg/encoder.rs:398`). JPEG has no alpha, so flatten onto white first.
- EXIF orientation: `ImageDecoder::orientation()` defaults to reading the EXIF chunk (`io/decoder.rs:53-58`; overridden by jpeg `:140`, webp `:95` and tiff `:337`). Then call `DynamicImage::apply_orientation(o)` (`images/dynimage.rs:1161`). Flow: `ImageReader::new(BufReader)`, `.with_guessed_format()` (`io/image_reader_type.rs:254`), `.into_decoder()` (`:219`), `let o = dec.orientation()?`, `DynamicImage::from_decoder(dec)?`, `img.apply_orientation(o)`.
- ICC: `ImageDecoder::icc_profile()` plus `ImageEncoder::set_icc_profile()` for png, jpeg, webp and tiff (`png.rs:780`, `jpeg/encoder.rs:752`, `webp/encoder.rs:96`, `tiff.rs:612`). Carry the profile over and ignore `UnsupportedError`. Default decode limit: `max_alloc` 512 MiB (`io/limits.rs:54`). Keep it; it guards against decompression bombs.
- Test helper: `JpegEncoder::set_exif_metadata` (`jpeg/encoder.rs:757`) can write an Orientation=6 EXIF for the orientation test.
- **No-clobber finalize without `unsafe` or new dependencies:** call `std::fs::hard_link(tmp, final)`, which is `CreateHardLinkW` on Windows and `link(2)` on Unix. It fails with `ErrorKind::AlreadyExists` if `final` exists, on NTFS and every Unix filesystem, and makes the name appear atomically with the full content. Then `remove_file(tmp)`. If hard links are unsupported (FAT32/exFAT, some SMB shares; any error other than `AlreadyExists`), fall back to `OpenOptions::new().write(true).create_new(true).open(final)`, which is an exclusive create, then copy, `sync_all`, and remove `tmp`. If the copy fails, remove the partial `final` (we created it, so deleting it is safe). That fallback is no-clobber but not atomic; it's acceptable and noted in D16. `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` would need `unsafe` in engines, which `#![forbid(unsafe_code)]` rules out.

---

## Work plan and file ownership (section I, decided up front)

**Decision: Wave 0 (contracts, done by the orchestrator, about 20 minutes), then four agents in parallel.** Why: the app agent has the biggest job and depends on platform and engines APIs. If the orchestrator first commits the *signatures* as compiling stubs (verbatim below), all four agents can build and test against real types from the first minute. Each agent owns disjoint files, so merges are mechanical. Running app second would serialize the longest task behind two others.

### Wave 0 (orchestrator commits on `claude/vigilant-gauss-l5qt5y`, then branches 4 worktrees)
1. `crates/grape-platform/src/lib.rs`: add `DragSignal::Escape`, `DropSignal`, `MonitorConfig`, `drag_monitor_with` (§A contract). Point `drag_monitor()` at `drag_monitor_with(MonitorConfig::default())`. Each `imp` module gets `drag_monitor_with(_cfg)`, which returns `Unsupported` for now.
2. `crates/grape-platform/src/windows/mod.rs`: add `DropPolicy` and `install_drop_target` (stub returning `Err(Unsupported)`), and `shell_drag_image_visible()` (stub returning `false`).
3. `crates/grape-engines/src/lib.rs` and `Cargo.toml`: the full §E public types, with `run_job` returning `NotYetSupported` and `supports` returning `false`. Add `thiserror.workspace = true`.
4. Run `cargo build --workspace` and commit `Cargo.lock` (only engines' thiserror edge changes).

### Ownership (no overlaps)
| Agent | Owns (may create/edit) | Must not touch |
|---|---|---|
| **1 Platform** | `crates/grape-platform/**`, including its `CLAUDE.md` | anything else |
| **2 Engines** | `crates/grape-engines/**`, **root `Cargo.toml`** (only if a workspace dependency is wanted; prefer crate-local), **root `Cargo.lock`** | anything else |
| **3 App** | `apps/desktop/src-tauri/**` (code, `Cargo.toml`, `tauri.conf.json`, `capabilities/`, `CLAUDE.md`) | `Cargo.lock`: let cargo update it locally but **don't commit it** (`git checkout Cargo.lock` before each commit); the orchestrator regenerates it after the merge |
| **4 UI** | `apps/desktop/ui/**` (including `package.json`, `vite.config.ts`, new `toast.html`, its `CLAUDE.md`), **root `pnpm-lock.yaml`** | anything else |
| **Orchestrator** | `docs/**`, `README.md`, root `CLAUDE.md`, `.github/**`, Wave 0, merge, final `Cargo.lock` | — |

Merge order: platform, engines, UI, app. After the app merge: `cargo build --workspace`. Check that the `Cargo.lock` diff contains only the `grape` package's dependency list plus engines' image tree, then run the full check list (§J).

---

## A. Platform agent: `crates/grape-platform`

### A.1 Contract (Wave 0 adds it; keep it verbatim)
```rust
// lib.rs
pub enum DragSignal {            // derive(Debug, Clone, Copy, PartialEq, Eq) as today
    Started,
    Moved { x: i32, y: i32 },
    Modifiers(Modifiers),
    /// Esc went down during a drag (edge, sent once per press).
    Escape,
    Ended,
}

/// What a Grape window's drop target saw. Global, top-left origin, physical px.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropSignal {
    /// A drag carrying files entered the window (CF_HDROP present).
    Enter { paths: Vec<std::path::PathBuf>, x: i32, y: i32 },
    /// A drag without files entered (text, browser image, virtual items). Grape refuses it.
    EnterNoFiles { x: i32, y: i32 },
    /// The cursor moved over the window (sent only when the position changed).
    Over { x: i32, y: i32 },
    /// The drag left the window or was cancelled.
    Leave,
    /// Files were dropped. The target has already answered DROPEFFECT_COPY.
    Drop { paths: Vec<std::path::PathBuf>, x: i32, y: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MonitorConfig {
    /// Arm for drags that start in any app, not only Explorer and the Desktop.
    pub any_app: bool,
}

pub fn drag_monitor_with(config: MonitorConfig) -> Result<Box<dyn DragMonitor>, PlatformError>;
pub fn drag_monitor() -> Result<Box<dyn DragMonitor>, PlatformError>; // = with(default)
pub fn display_bounds_at(x: i32, y: i32) -> Result<DisplayBounds, PlatformError>; // existing signature

// windows/mod.rs (cfg(windows) only)
pub enum DropPolicy {
    /// The overlay: accept file drags as DROPEFFECT_COPY and report everything.
    Accept(std::sync::mpsc::SyncSender<crate::DropSignal>),
    /// Settings / toast: answer DROPEFFECT_NONE, so OLE never calls Drop.
    Refuse,
}
/// Replaces every drop target on `hwnd` and its in-process descendants
/// (tao's and wry's) with Grape's. Returns how many HWNDs were registered.
/// Call it on the thread that owns `hwnd` (Tauri's main thread).
pub fn install_drop_target(hwnd: isize, policy: DropPolicy) -> Result<usize, PlatformError>;
/// True if the shell's drag image window ("SysDragImage") is visible. Diagnostic only.
pub fn shell_drag_image_visible() -> bool;
```
Pausing isn't a platform API: the app pauses with `stop()` and resumes with `start()` (the trait already supports restarts). Unhooking is the cleanest way to behave "as if Grape weren't installed".

### A.2 Pure detector: `src/detector.rs` (OS-neutral, `pub mod detector;`, unit-tested on Linux)
```rust
pub struct DetectorConfig {
    pub drag_threshold: (i32, i32),     // (SM_CXDRAG, SM_CYDRAG); default (4, 4)
    pub any_app: bool,
    pub allow_classes: &'static [&'static str], // ALLOW_CLASSES
}
pub const ALLOW_CLASSES: &[&str] = &["CabinetWClass", "ExploreWClass", "Progman", "WorkerW"];

/// Where the button went down, as seen by the glue.
pub enum Origin { OwnProcess, Window { root_class: String }, Unknown }

pub enum RawInput {
    MouseDown { x: i32, y: i32, origin: Origin },   // logical left button
    MouseMove { x: i32, y: i32 },
    MouseUp { x: i32, y: i32 },
    Keys(KeySnapshot),
}
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct KeySnapshot { pub mods: crate::Modifiers, pub escape: bool, pub left_button: bool }

pub struct Detector { /* Idle | Pending{down} | Dragging{last_mods, esc_down} */ }
impl Detector {
    pub fn new(cfg: DetectorConfig) -> Self;
    pub fn feed(&mut self, input: RawInput, out: &mut Vec<crate::DragSignal>);
    /// True from an allowlisted MouseDown until MouseUp: the glue polls keys only then.
    pub fn wants_key_polling(&self) -> bool;
    pub fn is_dragging(&self) -> bool;
}
```
Rules:
- `MouseDown`: arm (`Pending`) only if `origin` is a `Window` whose `root_class` is in `allow_classes`, or `any_app` is on and the origin isn't `OwnProcess`. Otherwise stay idle. A `MouseDown` while already armed resets to the new down.
- `MouseMove` in `Pending` past the threshold emits `Started`, `Moved{x, y}` and then `Modifiers(latest snapshot mods)` (the **baseline**), and moves to `Dragging`. In `Dragging`, `MouseMove` emits `Moved`. The glue coalesces moves, so `feed` may emit every one.
- `MouseUp` in `Dragging` emits `Ended` and goes `Idle`. In `Pending`, it goes `Idle` silently.
- `Keys` in `Dragging`: emit `Modifiers(m)` when `m` changed, and `Escape` on an `escape` false→true edge. **`left_button == false` while `Dragging` emits `Ended`** (a missed up, for example after the hook was dropped). In `Pending`, `left_button == false` goes `Idle`.
- Unit tests, with no OS: allowlist hit and miss, `any_app`, own process, threshold boundaries (exactly cx doesn't start; cx+1 does), baseline modifiers emitted once, an Esc edge emitted once per press, a missed-up recovery, Started/Ended pairing, and replaying each `mock::scripts` sequence.

### A.3 Windows glue: `src/windows/hook.rs`, `drag.rs`, `display.rs`, `drop_target.rs`
- **hook.rs**: a thread named `grape-mouse-hook` at `THREAD_PRIORITY_HIGHEST`. It sets `HOOK_TX`, installs `WH_MOUSE_LL` and pumps `GetMessageW`. The callback: if `n_code == HC_ACTION`, map `WM_LBUTTONDOWN`, `WM_LBUTTONUP` and `WM_MOUSEMOVE` to `RawMouse{kind, x: pt.x, y: pt.y}` and `try_send` it. Keep a `static LEFT_DOWN: AtomicBool`, and send moves **only while the left button is down**. Always return `CallNextHookEx(None, n_code, w, l)`. Never swallow input.
- **drag.rs** (`WindowsDragMonitor: DragMonitor`): `start(tx)` spawns the hook thread and a `grape-drag-detector` thread that owns the `Detector`. Loop: `rx.recv_timeout(if detector.wants_key_polling() { 8 ms } else { 1 s })`, then drain everything queued. For a down event, resolve `Origin` here, not in the hook: `WindowFromPoint`, `GetAncestor(GA_ROOT)`, `GetClassNameW`, plus the own-pid check. Coalesce moves to the last one, feed, poll `GetAsyncKeyState` while `wants_key_polling()`, feed `Keys`, and `try_send` each `DragSignal` to the app (drop it if full; log at debug).
  - **Hook health check (should):** while idle, every 1 s compare `GetCursorPos` with the last position seen by the hook. If the cursor moved more than 50 px and the hook delivered nothing for 2 s, log a warning, then unhook and reinstall (Windows removes slow hooks silently).
  - At `Started`, log `drag start at (x,y) root=<class> shell_drag_image=<bool>`.
  - `stop()`: set a flag, `PostThreadMessageW(WM_QUIT)` to the hook thread, join both threads, clear `HOOK_TX`. It must be idempotent, and `start` after `stop` must work.
- **display.rs**: `display_bounds_at` as in §1.5. `DisplayBounds.monitor` = `rcMonitor` and `work_area` = `rcWork`, both physical. If `GetDpiForMonitor` fails, fall back to `scale = 1.0` and log it.
- **drop_target.rs**: `GrapeDropTarget { policy, has_files: Cell<bool>, last_pos: Cell<Option<(i32,i32)>> }` with `#[implement(IDropTarget)]`.
  - `DragEnter`: read `CF_HDROP` via `GetData`, then `DragQueryFileW`, then **`ReleaseStgMedium`**.
    - **Accept:** with files, `*pdwEffect = DROPEFFECT_COPY` and send `Enter`. Without files, `*pdwEffect = DROPEFFECT_NONE` and send `EnterNoFiles`.
    - **Refuse:** always `*pdwEffect = DROPEFFECT_NONE`, send nothing.
  - `DragOver`: **always write** `*pdwEffect` (COPY if `has_files && Accept`, otherwise NONE). Send `Over` only when the position changed.
  - `DragLeave`: send `Leave` if it was entered with files.
  - `Drop`: **always write `*pdwEffect = DROPEFFECT_COPY`** (Accept with files) or `DROPEFFECT_NONE` (otherwise). Re-read the paths, send `Drop`, return `Ok(())`.
  - Ignore `grfKeyState`. Never return MOVE or LINK. Every callback does only `try_send`; no blocking, no logging at info level inside COM calls.
  - `install_drop_target(hwnd, policy)`:
    - Refuse with `WrongThread` unless `GetWindowThreadProcessId(hwnd) == GetCurrentThreadId()`.
    - Collect `hwnd` plus `EnumChildWindows(hwnd)`, keeping only windows of the current thread.
    - For each: `RevokeDragDrop` (ignore `DRAGDROP_E_NOTREGISTERED`), then `RegisterDragDrop(h, target)`. Use one target object per HWND, or one shared; both are fine.
    - Return the count, and `Err(Os{…})` if it's zero.
    - Ignore `DRAGDROP_E_INVALIDHWND` on children, as wry does.
  - **Optional, only after everything above works:** call `IDropTargetHelper` (`CLSID_DragDropHelper`) `DragEnter`/`DragOver`/`DragLeave`/`Drop` so Explorer's drag thumbnail stays visible over the overlay. Failures are ignored.
- **Windows-only tests (they run in CI on `windows-latest`):**
  - (1) `display_bounds_at(0, 0)` gives `scale > 0` and a work area inside the monitor.
  - (2) Start/stop of the monitor, three times, without hanging.
  - (3) **Golden-rule-2 scripted check.** Create a hidden test window on the test thread (`OleInitialize`), build a `GrapeDropTarget(Accept)` and a minimal `#[implement(IDataObject)]` that serves `CF_HDROP` (a `DROPFILES` struct in `GlobalAlloc` memory). Call `DragEnter`, `DragOver` and `Drop` directly with `*pdwEffect` preset to `COPY|MOVE|LINK`. Assert that the result is **exactly `DROPEFFECT_COPY`** after each call and that `Enter`/`Drop` carried the path. Repeat with `Refuse` (NONE) and with a data object that has no `CF_HDROP` (NONE, `EnterNoFiles`).
  - (4) `install_drop_target` on a test window returns at least 1, and from another thread it returns `WrongThread`.
- Update `lib.rs`'s `entry_points_are_unsupported_until_m1` test: it stays as is on non-Windows; on Windows it asserts the entry points now work.
- Extend `mock::scripts` with `escape_mid_drag()` (Started, Moved, Modifiers(SHIFT), Modifiers(NONE), Escape, Ended) and `drag_with_shift_held_from_start()` (Started, Moved, Modifiers(SHIFT) as the baseline, Moved, Ended). Keep the existing tests green.
- Run on Linux: `cargo test -p grape-platform`, `cargo clippy -p grape-platform --all-targets -- -D warnings` and `cargo clippy -p grape-platform --all-targets --target x86_64-pc-windows-msvc -- -D warnings` (the target is installed in the container). Every `unsafe` block gets a `// SAFETY:` comment.

---

## B. App agent: `WheelController` (pure) in `apps/desktop/src-tauri/src/controller.rs`

No Tauri types inside it. Test it with `grape_platform::mock::scripts` plus hand-built `DropSignal`s and a fake `Env`.

### B.1 Types
```rust
use grape_core::{Features, Platform, Point, Rect as LRect, SelectionItem, WheelContent, WheelEntry, WheelMode};
use grape_platform::{DisplayBounds, DragSignal, DropSignal, Modifiers};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key { Shift, Ctrl, Alt, Meta }
pub struct KeyMap { pub conversions: Key, pub tools: Key, pub actions: Key }   // default Shift/Ctrl/Alt
impl KeyMap { pub fn mode_for(&self, k: Key) -> Option<WheelMode>; pub fn names(&self) -> KeyNames; }
/// Exactly-one-modifier rule plus key-down edge: Some(mode) iff `now` holds exactly one
/// modifier, that key is mapped, and it was not held in `prev`.
pub fn edge_mode(prev: Modifiers, now: Modifiers, keys: &KeyMap) -> Option<WheelMode>;

pub struct ControllerConfig {
    pub keys: KeyMap, pub features: Features, pub platform: Platform, pub sounds: bool,
    pub no_paths_timeout: Duration,   // 300 ms
    pub no_paths_cap: Duration,       // 1500 ms after open
    pub drop_grace: Duration,         // 750 ms after Ended while inside
    pub watchdog: Duration,           // 30 s
}
pub trait Env {
    fn display_at(&self, x: i32, y: i32) -> Option<DisplayBounds>;
    /// fs::metadata per path: folder → SelectionItem::folder, else SelectionItem::file.
    fn classify(&self, paths: &[std::path::PathBuf]) -> Vec<SelectionItem>;
}
pub enum Input { Drag(DragSignal), Drop(DropSignal), Tick, SetPaused(bool) }
pub enum Effect {
    /// Move and resize the hidden overlay to this monitor (global physical).
    PrepareOverlay { monitor: grape_platform::Rect },
    ShowOverlay,
    HideOverlay,
    Emit(UiEvent),
    Run(RunRequest),
}
pub enum UiEvent { Open(WheelOpen), Highlight(WheelHighlight), Close(WheelClose) } // payloads in §C
pub struct RunRequest {
    pub seq: u64, pub items: Vec<SelectionItem>, pub entry: WheelEntry,
    pub anchor_phys: (i32, i32), pub display: DisplayBounds,
}
pub struct WheelController { /* state, cfg, seq counter, prepared monitor */ }
impl WheelController {
    pub fn new(cfg: ControllerConfig) -> Self;
    pub fn handle(&mut self, now: Instant, input: Input, env: &dyn Env) -> Vec<Effect>;
    /// Earliest timer; the loop does recv_timeout(until it) and feeds Input::Tick.
    pub fn next_deadline(&self) -> Option<Instant>;
    pub fn state_name(&self) -> &'static str;   // for logs and tests
}
```

### B.2 States and transitions (INTERACTION_SPEC §1, with the Windows notes above)
States: `Idle`, `Dragging{mods, cursor: Option<(i32,i32)>}`, `WheelOpen(Open)`, `AwaitingDrop(Open, deadline)` and `Paused`. Here `Open = { seq, mode, mods, anchor_phys, display, anchor_l: Point, items: Option<Vec<SelectionItem>>, content: Option<WheelContent>, inside: bool, highlight: Option<usize>, opened_at, no_paths_deadline, last_input }`. `Running(job)` from the spec is represented by `Effect::Run` followed by an immediate return to `Idle`. The job runs on a worker and feeds back only through toasts, so a new drag can start while it runs.

| From | Input | Effects / next |
|---|---|---|
| any but Paused | `SetPaused(true)` | if the overlay is visible: `HideOverlay, Emit(Close{paused})`. Go to `Paused` |
| Paused | `SetPaused(false)` | `Idle`. All other inputs are ignored while Paused |
| Idle | `Drag(Started)` | `Dragging{mods: NONE, cursor: None}` |
| Dragging | `Moved{x,y}` | Store the cursor. If `env.display_at(x,y)` is a different monitor from the prepared one: `PrepareOverlay{monitor}` (pre-positioning keeps key → visible under 100 ms) |
| Dragging | `Modifiers(m)` | `if let Some(mode) = edge_mode(prev, m)` and the cursor is known, **open**: `display = env.display_at(cursor)`. If that's `None`, log it and stay in `Dragging` (golden rule 4). Compute `anchor_l = clamp_anchor(to_local(cursor), work_area_local)` and `anchor_phys = to_global(anchor_l)`, and `seq += 1`. Effects: `[PrepareOverlay?] , Emit(Open{content: pending}), ShowOverlay`. Next: `WheelOpen`. **Always** update `mods = m` |
| Dragging | `Ended` / `Escape` | `Ended` → `Idle`. `Escape` → `Dismissed`: the source cancelled the drag, so no wheel until the button goes up (D18) |
| Dragging / Idle | any `Drop(..)` | Ignore and log at warn (the overlay is hidden) |
| WheelOpen | `Modifiers(m)` | If `edge_mode` gives a mode other than the current one: **swap in place**, same anchor and same seq, recompute the content if items are known, reset the highlight, `Emit(Open{…})`. Update `mods` |
| WheelOpen | `Moved{x,y}` or `Drop(Over{x,y})` | If the content has n>0 entries: `hit = hit_test(dx, dy, n)` (§B.3). If it changed: `Emit(Highlight{index})`. A first move after open extends `no_paths_deadline` (see Tick) |
| WheelOpen | `Drop(Enter{paths,x,y})` | `inside = true`. `items = env.classify(&paths)`, `content = wheel_content(&items, mode, features)`, `Emit(Open{entries/empty})`, then hit-test (x,y). Log the paths, the position and the content size |
| WheelOpen | `Drop(EnterNoFiles{..})` | Close with `noFiles`: `HideOverlay, Emit(Close)`, `Idle`. OLE re-targets the window below on its next tick, so non-file drags aren't blocked |
| WheelOpen | `Drop(Leave)` | `inside = false`, highlight `None` (emit if changed). Keep the wheel open (cursor on another monitor, or OS cancel) |
| WheelOpen / AwaitingDrop | `Drop(Drop{paths,x,y})` | Hit-test (x,y). On `Some(i)` with `entry = content.entries()[i]`: `HideOverlay, Emit(Close{drop}), Run(RunRequest)`. Otherwise (dead zone, gap, off-ring, empty or pending wheel): `HideOverlay, Emit(Close{cancel})`, a **swallowed** drop. Either way → `Idle`. Log the paths, the position, `(dx,dy)` in logical units and the hit |
| WheelOpen | `Drag(Escape)` | `HideOverlay, Emit(Close{escape})`, `Idle`. Windows cancels the OS drag itself (§0.2). A later `Ended` in `Idle` is ignored |
| WheelOpen | `Drag(Ended)` | If `inside`: `AwaitingDrop(deadline = now + drop_grace)` and **keep the overlay up**. Otherwise: `HideOverlay, Emit(Close{dragEnded})`, `Idle` |
| AwaitingDrop | `Drop(Leave)` or Tick past the deadline | `HideOverlay, Emit(Close{dragEnded})`, `Idle` |
| WheelOpen | `Tick` | `items == None && now >= no_paths_deadline` → close `noFiles`. `now - last_input >= watchdog` → close `watchdog`. `no_paths_deadline` starts at `opened_at + 300 ms`; the first `Moved`/`Over` after open sets it to `max(current, t + 300 ms)`, capped at `opened_at + 1500 ms` |
| any | `Drag(Started)` while not Idle | A new drag: close anything visible (`dragEnded`), then handle it as from `Idle` |

`last_input` is updated on every non-Tick input. The watchdog only runs while the overlay is visible.

### B.3 Coordinates (exact)
- **G** = global physical px, top-left origin: the hook's `Moved`, `DropSignal` positions, `DisplayBounds`. **M** = `display.monitor`, **W** = `display.work_area`, **s** = `display.scale`.
- Overlay-local logical (= CSS px in the overlay webview, whose zoom equals `s` under PMv2): `to_local(g) = Point{ x: (g.x − M.x) as f64 / s, y: (g.y − M.y) as f64 / s }`.
- Work area in local logical: `LRect{ x: (W.x−M.x)/s, y: (W.y−M.y)/s, width: W.width/s, height: W.height/s }`.
- Anchor: `anchor_l = grape_core::clamp_anchor(to_local(cursor_at_activation), work_local)`. `anchor_phys = (M.x + round(anchor_l.x·s), M.y + round(anchor_l.y·s))`; it's used only to place the toast.
- Hit test: `let p = to_local(g); hit_test(p.x − anchor_l.x, p.y − anchor_l.y, n)`. Both offsets are logical units, y down, as `grape_core::hit_test` expects.
- The overlay window rect is exactly `M` (physical). The UI draws the wheel centered at `anchor_l` in CSS px.
- Put these helpers in `src/coords.rs`, pure. Tests: scale 1.0, 1.25, 1.5 and 2.0, a monitor at negative origin (left of primary), work area with taskbar on the left or top, and anchors clamped at every edge.

### B.4 Controller tests (Linux, `cargo test -p grape`)
- Fake `Env`: one or two monitors (one at 150%), and `classify` from a map.
- Cases:
  - Plain drag: no effects beyond Prepare.
  - **Shift held from the start** (`drag_with_shift_held_from_start`): never opens.
  - Shift tap opens conversions with a pending wheel; Enter brings entries; Over highlights; Drop on a segment gives `Run` with the right `WheelEntry`.
  - Drop in the dead zone, in a gap and off-ring: cancel, no `Run`.
  - Key swap keeps the anchor and seq.
  - Two modifiers: no open.
  - `EnterNoFiles`: close.
  - No Enter within 300 ms: close; with the first-move extension; with the 1.5 s cap.
  - Esc: close.
  - Ended while inside, then Drop within the grace: `Run`.
  - Ended while inside with no drop: close at the deadline.
  - Ended while not inside: immediate close.
  - Leave keeps the wheel open.
  - Watchdog at 30 s.
  - Paused ignores everything and closes an open wheel.
  - A folder shows the Actions wheel; the Conversions wheel shows `empty/noConversions`.
  - Clamping near a screen corner.
  - A mixed-DPI anchor on the second monitor.

---

## C. Event contract (Rust → UI, plus two UI → Rust commands)

Rust structs go in `apps/desktop/src-tauri/src/events.rs`, all `#[derive(Serialize, Clone)]` with `#[serde(rename_all = "camelCase")]`. TS mirrors go in `apps/desktop/ui/src/shared/events.ts`, with runtime type guards over `unknown`. **Both sides test against the golden JSON below verbatim:** Rust with `serde_json::to_value == serde_json::from_str(GOLDEN)`, TS with `JSON.parse(GOLDEN)` passing the guard.

Events (all emitted with `emit_to(label, name, payload)`):

| Event | To | Payload |
|---|---|---|
| `wheel://open` | `overlay` | `WheelOpen`: render or re-render (swap, entries arrived) |
| `wheel://highlight` | `overlay` | `{ seq, index: number \| null }` |
| `wheel://close` | `overlay` | `{ seq, reason }`: clear the DOM immediately |
| `toast://show` | `toast` | `{ id, tone, title, detail, progress, revealable }`: replaces whatever is shown |
| `toast://hide` | `toast` | `{ id }` |

Rust:
```rust
pub struct WheelOpen { pub seq: u64, pub mode: WheelMode, pub anchor: Point, pub scale: f64,
    pub geometry: grape_core::Geometry, pub keys: KeyNames, pub sounds: bool, pub content: UiContent }
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum UiContent { Pending, Entries { dense: bool, segments: Vec<UiSegment> }, Empty { state: grape_core::EmptyState } }
pub struct UiSegment { #[serde(flatten)] pub geo: grape_core::Segment, pub title: String, pub entry: WheelEntry }
pub struct KeyNames { pub conversions: String, pub tools: String, pub actions: String }   // "Shift","Ctrl","Alt"
pub struct WheelHighlight { pub seq: u64, pub index: Option<usize> }
pub struct WheelClose { pub seq: u64, pub reason: CloseReason }
#[serde(rename_all = "camelCase")] pub enum CloseReason { Drop, Cancel, Escape, NoFiles, DragEnded, Watchdog, Paused }
pub struct ToastShow { pub id: u64, pub tone: ToastTone, pub title: String, pub detail: Option<String>,
    pub progress: Option<f32>, pub revealable: bool }
#[serde(rename_all = "camelCase")] pub enum ToastTone { Working, Done, Error, Info }
pub struct ToastHide { pub id: u64 }
```
`dense = n >= GEOMETRY.dense_at`. `segments` come from `wheel_layout(n)` zipped with the entries. `title = entry.title(Platform::Windows)`.

TS:
```ts
export type WheelMode = "conversions" | "tools" | "actions";
export interface Point { x: number; y: number }
export interface Geometry { innerR: number; outerR: number; backdropR: number; gap: number; corner: number; labelR: number; denseAt: number; screenMargin: number }
export type WheelEntry = { kind: "convert" | "tool" | "action"; key: string };
export interface UiSegment { index: number; center: number; start: number; end: number; labelX: number; labelY: number; title: string; entry: WheelEntry }
export type EmptyState = "noConversions" | "noTools";
export type WheelContent =
  | { kind: "pending" }
  | { kind: "entries"; dense: boolean; segments: UiSegment[] }
  | { kind: "empty"; state: EmptyState };
export interface KeyNames { conversions: string; tools: string; actions: string }
export interface WheelOpenPayload { seq: number; mode: WheelMode; anchor: Point; scale: number; geometry: Geometry; keys: KeyNames; sounds: boolean; content: WheelContent }
export interface WheelHighlightPayload { seq: number; index: number | null }
export type CloseReason = "drop" | "cancel" | "escape" | "noFiles" | "dragEnded" | "watchdog" | "paused";
export interface WheelClosePayload { seq: number; reason: CloseReason }
export type ToastTone = "working" | "done" | "error" | "info";
export interface ToastShowPayload { id: number; tone: ToastTone; title: string; detail: string | null; progress: number | null; revealable: boolean }
export interface ToastHidePayload { id: number }
```

**Golden JSON** (paste into both test suites):
```json
{"seq":1,"mode":"conversions","anchor":{"x":400.0,"y":300.0},"scale":1.5,"geometry":{"innerR":48.0,"outerR":120.0,"backdropR":126.0,"gap":4.0,"corner":20.0,"labelR":84.0,"denseAt":7,"screenMargin":14.0},"keys":{"conversions":"Shift","tools":"Ctrl","actions":"Alt"},"sounds":true,"content":{"kind":"pending"}}
```
```json
{"seq":2,"mode":"conversions","anchor":{"x":400.0,"y":300.0},"scale":1.0,"geometry":{"innerR":48.0,"outerR":120.0,"backdropR":126.0,"gap":4.0,"corner":20.0,"labelR":84.0,"denseAt":7,"screenMargin":14.0},"keys":{"conversions":"Shift","tools":"Ctrl","actions":"Alt"},"sounds":false,"content":{"kind":"entries","dense":false,"segments":[{"index":0,"center":1.5,"start":2.0,"end":1.0,"labelX":0.0,"labelY":-84.0,"title":"PNG","entry":{"kind":"convert","key":"png"}}]}}
```
```json
{"seq":3,"mode":"tools","anchor":{"x":400.0,"y":300.0},"scale":1.0,"geometry":{"innerR":48.0,"outerR":120.0,"backdropR":126.0,"gap":4.0,"corner":20.0,"labelR":84.0,"denseAt":7,"screenMargin":14.0},"keys":{"conversions":"Shift","tools":"Ctrl","actions":"Alt"},"sounds":true,"content":{"kind":"empty","state":"noTools"}}
```
```json
{"seq":1,"index":0}
{"seq":1,"index":null}
{"seq":1,"reason":"noFiles"}
{"id":4,"tone":"working","title":"Making photo.png…","detail":null,"progress":null,"revealable":false}
{"id":4,"tone":"done","title":"Done","detail":"photo.png","progress":null,"revealable":true}
{"id":4}
```
(The Rust test for the entries payload builds `UiSegment` by hand with these round numbers, not from `wheel_layout`.)

**UI → Rust commands** (app commands are allowed for all windows by default when no app manifest restricts them; keep it that way):
- `wheel_painted(seq: u64)`: the overlay calls it after the first `requestAnimationFrame` following a render. Rust logs `wheel seq=N painted +X ms after key edge`. That's the M1 latency measurement.
- `toast_clicked(id: u64)`: Rust reveals the first output of that batch if it's `revealable`. **The UI never sends paths.**

**Copy (Grape's own words).** The UI owns the empty-state strings; Rust owns the toast strings.
- Empty `noConversions`: `Nothing to convert here. Switch to file actions with {actions}.`
- Empty `noTools`: `No tools fit here. Switch to conversions with {conversions} or file actions with {actions}.`
- (Reworded from the first draft, whose second sentences echoed the observed copy in `INTERACTION_SPEC.md` §5; `ui/src/wheel/copy.ts` owns these strings.)
- Pending center label: `…`
- Toasts:
  - Working, one file: `Making {name}…`. Several files: `Making {n} files…`.
  - Done: title `Done`, detail `{name}` (or `{n} files`).
  - Partial: title `{ok} of {n} done`, detail = the first error's `user_message`.
  - Error: title = `user_message`, detail null.
  - Fallback, with the location first so the ellipsis can't cut it: when every output fell back, title `Saved in Downloads\Grape`, detail `{name}; its folder is read-only` (or `{n} files; their folders are read-only`). When only some did, title `Done`, detail `{n} files; {k} in Downloads\Grape`.
  - Not in this build: info, title `Not in this build yet`, detail `{FROM} to {TO} is coming.` (The detail is one line cut with an ellipsis, about 248 px at 12 px, so a list of the formats that do convert wouldn't be seen.)
  - Tools: info, `Tools arrive in a later build.` Actions: info, `File actions arrive in a later build.`
  - (The "Making {name}…" / "Done" wording follows ROADMAP M3; its clean-room status falls under open question O7.)

---

## D. UI agent: `apps/desktop/ui`

Files:
- `src/shared/events.ts` (types plus guards)
- `src/wheel/geometry.ts` (pure)
- `src/wheel/copy.ts` (pure)
- `src/wheel/tick.ts`
- `src/wheel/render.ts` (DOM)
- `src/overlay.ts`
- `overlay.html`, rewritten with no hard-coded radii
- `toast.html`, `src/toast.ts`, `src/toast/view.ts`
- `src/styles/overlay.css` and `src/styles/toast.css`, reusing `tokens.css`
- Tests: `geometry.test.ts`, `copy.test.ts`, `events.test.ts` (golden JSON)
- `package.json`: add `"@tauri-apps/api": "2.12.1"` (exact) and run `pnpm install` (respecting `minimumReleaseAge`).
- `vite.config.ts`: add a `toast` entry.
- Keep `no-any.test.ts` passing; use `unknown` plus guards.

### D.1 Segment path, pure (`geometry.ts`)
`segmentPath(seg: UiSegment, n: number, g: Geometry): string` returns an SVG path in a coordinate system centered on the anchor (y down).
- Math angle θ maps to screen `(r cosθ, −r sinθ)`.
- Half-width `h = π/n`. `n === 1` gives a full annulus: two circles (outer clockwise, inner counter-clockwise, `fill-rule: evenodd`), no gaps or corners.
- Each radial edge is a straight line **parallel** to the boundary ray at angle `β = center ± h`, offset by `g.gap/2` toward the segment. That matches the core hit test exactly (`|Δθ| ≤ π/n − asin(GAP/2 / d)`).
- Without rounding, the corner angles at radius r are `center ± (h − asin(gap/2 / r))`, for r = innerR and r = outerR.
- **Outer corners rounded with radius ρ = min(g.corner, ρmax).** In a frame rotated so the boundary ray is +u and the segment lies at v<0 (math orientation), the fillet center is `(√((R−ρ)² − (gap/2+ρ)²), −(gap/2+ρ))`. The tangent point on the edge is `(u_c, −gap/2)`, and on the outer circle it is `center · R/(R−ρ)`. Rotate back by β and mirror for the other edge. `ρmax` is the largest ρ (binary search, 20 iterations) for which the outer tangent point's angle from β stays at most `h − 1e-6`, so the two fillets never cross on narrow segments.
- Inner corners stay square.
- Path order: outer arc (clockwise in screen space) → fillet → edge line → inner arc back → edge line → fillet → close.
- Export `segmentCorners(...)` for tests.
- Tests:
  - Every vertex lies on the gap line or on the circles (within 1e-6).
  - `n = 1..12` produce valid paths (no NaN).
  - Segment 0 is centered at the top for n≠3 and on the left for n=3, using the Rust-provided `center`, so no angle math is duplicated.
  - Adjacent segments are exactly `gap` apart along a radius line.
  - `ρ` shrinks for n=12.
  - Label anchor = `(labelX, labelY)`.

### D.2 Rendering (`render.ts`, `overlay.ts`)
- Listen with `listen<unknown>("wheel://open" | "wheel://highlight" | "wheel://close", …)` from `@tauri-apps/api/event`, then guard, then render. Keep `currentSeq` and ignore highlights or closes for other seqs.
- Root: `<svg id="wheel">` absolutely positioned with `left = anchor.x − backdropR` and `top = anchor.y − backdropR`, size `2·backdropR`, viewBox centered.
- Draw the backdrop circle (`--glass-fill`, `--glass-stroke`), the segment paths (`--segment-fill`, `--segment-stroke`), the highlighted path (`--segment-highlight`), and labels as `<text>` at `(labelX, labelY)`: dense means a smaller font size and weight. Add `role="img"`, an `aria-label` per segment and a `<title>`.
- Center label: a `<text>` at the center showing the highlighted segment's title. Hidden when `index === null`. Shows `…` while pending.
- Pending: draw `n = 8` placeholder segments at low opacity (class `pending`) plus `…`.
- Empty: draw no segments; the backdrop shows the empty-state message (word-wrapped in `<foreignObject>` or `<text>` lines, at most 3 lines, from `copy.ts` with the payload's `keys`).
- Close: remove all children at once (no animation), so the next show never flashes a stale wheel.
- `prefers-reduced-motion: reduce`: no animation. Otherwise a 90 ms scale (0.94 → 1) and opacity-in on open, and none on swap.
- **Hit layer:** `html, body { background: var(--overlay-hit-fill) }`, with `--overlay-hit-fill: rgba(0, 0, 0, 0.004)` in `overlay.css` (D15). The body stays transparent to the eye.
- After rendering, `requestAnimationFrame` then `invoke("wheel_painted", { seq })`. Catch and ignore errors.
- Light/dark: the tokens already exist. Backdrop blur doesn't apply in a transparent window, so rely on the fill alpha.

### D.3 Tick (`tick.ts`), original synthesized sound
- One lazy `AudioContext`; `resume()` it on each open.
- On a highlight change to a **new non-null** index, when `sounds` is on: an oscillator `sine` at 1320 Hz to 990 Hz (exponential ramp over 25 ms), gain 0.06 to 0.0001 over 35 ms, stopped at 40 ms.
- A highlight of `null` cancels any tick scheduled within the next 10 ms (keep the node and `stop()` it).
- Never throw if audio is blocked: log once with `console.warn`.

### D.4 Toast (`toast.html`, `toast.ts`)
- A rounded glass pill, 320×64 CSS px window (the window size is set by Rust), using the tokens.
- Content: a tone dot (working: animated purple, unless reduced motion; done: leaf green; error: a red from the grape palette, which may be added to `tokens.css`; info: muted), the title, the detail, and a thin progress bar when `progress !== null`.
- Listen to `toast://show` and `toast://hide`. Clicking anywhere calls `invoke("toast_clicked", { id })` when `revealable`, with a pointer cursor only then.
- No other behavior.

Tests: `npm-free` pure functions only (vitest, node environment): geometry, copy (key substitution with any key names, e.g. "Caps"), and the guards against the golden JSON plus malformed variants.

Run: `pnpm -C apps/desktop/ui typecheck`, `pnpm -C apps/desktop/ui test`, `pnpm -C apps/desktop/ui build`.

---

## E. Engines agent: `crates/grape-engines`

### E.1 Public contract (Wave 0 stubs; keep it verbatim)
```rust
#![forbid(unsafe_code)]
use std::path::PathBuf;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use grape_core::{Format, Job, Operation};

/// Sidecar locations injected by the app. All None in this slice.
#[derive(Debug, Clone, Default)]
pub struct SidecarPaths { pub ffmpeg: Option<PathBuf>, pub ffprobe: Option<PathBuf>, pub heif_enc: Option<PathBuf> }

#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);
impl CancelToken {
    pub fn new() -> Self;
    pub fn cancel(&self);
    pub fn is_cancelled(&self) -> bool;
}

#[derive(Debug, Clone, Default)]
pub struct EngineCtx { pub sidecars: SidecarPaths, pub cancel: CancelToken, pub fallback_dir: Option<PathBuf> }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOutcome {
    /// Files/folders actually written (final paths).
    pub outputs: Vec<PathBuf>,
    /// True when the planned name was taken at finalize time and a " N" name was used.
    pub renamed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind { NotYetSupported, ReadFailed, DecodeFailed, EncodeFailed, WriteFailed, NameTaken, Cancelled }

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{user_message}")]
pub struct GrapeError { pub kind: ErrorKind, pub user_message: String, pub detail: String }

/// Can this build execute `op` for an input of format `from`?
pub fn supports(op: &Operation, from: Option<Format>) -> bool;

/// Runs one planned job. `progress` gets fractions in 0.0..=1.0 (may never be called).
/// Never opens a source for writing; never replaces an existing file; leaves no temp
/// file behind on error or cancel.
pub fn run_job(job: &Job, ctx: &EngineCtx, progress: &mut dyn FnMut(f32)) -> Result<JobOutcome, GrapeError>;
```

### E.2 Implementation
- `src/image.rs` handles `RASTER = [Jpg, Png, Webp, Tiff, Bmp]`. `supports(Convert{to}, Some(from))` is true iff both are in `RASTER` and `from != to`, which gives 20 pairs. Everything else (heic, avif, svg, pdf, audio, video, tools) is false.
- `run_job` checks:
  - `!supports(...)` → `NotYetSupported` with user message `"{FROM} to {TO} isn't in this build yet."` (display names from `Format::display_name`).
  - `JobOutput` must be `File`; otherwise `NotYetSupported`.
  - Exactly one input.
- Steps:
  1. If cancelled → `Cancelled`.
  2. `File::open(src)` (read-only) into a `BufReader`, then `ImageReader::new(..).with_guessed_format()`, `into_decoder()`, `orientation()`, `icc_profile()` (ok to fail), `DynamicImage::from_decoder`, `apply_orientation`. Report `progress(0.5)`.
     - **[Grape] Memory limits** (D16), all checked before any pixel is decoded: a JPEG or WebP file over 512 MiB is refused unread (PNG, TIFF and BMP files can be any size); the PNG decoder's metadata is held by two 128 MiB limits (its text and EXIF chunks, counted from their headers before decoding; and, by the decoder's own limit, the inflated ICC profile); an ICC profile over 255 × 65,519 bytes (what a JPEG can hold) is dropped for every target; the decoded image must fit the `image` crate's 512 MiB default; and the estimated peak of the whole conversion must fit 1 GiB. That estimate counts what the decoder keeps, read from the headers first (twice the JPEG file and, for any frame but a sequential gray one, its coefficients, with the headers read by zune-jpeg itself; the WebP frame, the alpha decoding of a still lossy image with alpha, or an animation's frame and canvas; the PNG metadata and, with an ICC profile, the decoder's limit), and the profile carried over.
  3. Convert per target:
     - **jpg:** if the image has alpha, composite onto white (`to_rgba8` then a manual blend), then `to_rgb8`. `JpegEncoder::new_with_quality(w, 90)`.
     - **png:** keep the color type.
     - **webp:** `to_rgba8` if it has alpha, else `to_rgb8`, then `WebPEncoder::new_lossless`.
     - **bmp:** `to_rgba8` or `to_rgb8`.
     - **tiff:** keep 8- and 16-bit, map others to rgba8/rgb8.
     - Set ICC when the encoder accepts it.
  4. Temp file in the destination folder: `.grape-tmp-{pid}-{nanos}-{counter}.{ext}`, opened with `create_new`. Write through a `BufWriter`, `flush`, `sync_all`, close. If cancelled now → remove the temp, `Cancelled`.
  5. **Finalize, no clobber** (§1.6): `hard_link`, then `remove(tmp)`. On `AlreadyExists` → re-plan. On any other error → the `create_new` copy fallback.
     - Re-plan: read the destination folder's names fresh, add every name attempted so far, `grape_core::output_name(source_file_name, &job.op, &taken)`, and retry up to 3 times with `renamed = true`. After that → `NameTaken`.
  6. Cleanup guard (a `Drop` struct) removes the temp on every error path. `progress(1.0)`.
- Errors map to user messages: `ReadFailed` "Grape couldn't read {name}.", `DecodeFailed` "{name} isn't a readable {FROM} image." (or "{name} is too large to convert." when a memory limit refuses it), `EncodeFailed` "Grape couldn't write a {TO} file.", `WriteFailed` "Grape couldn't save into {folder}.", `NameTaken` "A file named {name} keeps appearing; try again." `detail` = the `io` or `image` error's `Display`, plus paths. Never include file contents.
- Animated or multi-page inputs use the first frame or page (documented).

### E.3 Dependency (D16)
In `crates/grape-engines/Cargo.toml`:
```toml
image = { version = "0.25.10", default-features = false, features = ["jpeg", "png", "webp", "bmp", "tiff"] }
```
(crate-local; no root `Cargo.toml` change needed). Then check that no new lock entry is younger than 14 days (cutoff **2026-10-08**). For each `name version` newly added in `git diff Cargo.lock`:
```bash
curl -s "https://crates.io/api/v1/crates/$name/$version" -H 'User-Agent: grape-dev' | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["version"]["created_at"])'
```
If anything is too young: `cargo update -p <name> --precise <older>`. Record every new crate, its version and license in your summary for D16.

### E.4 Tests (`tests/image.rs`; no binary fixtures, everything generated into a unique dir under `std::env::temp_dir()` with a small `TestDir` helper that cleans up)
- Generate a 64×48 RGBA gradient with partial alpha and encode it to each input format using `image` itself.
- **All 20 pairs:** plan with `grape_core::plan` (build `Selection` and `DirSnapshot` by hand from `read_dir`), then `run_job`. Assert that:
  - the output exists and has the planned name;
  - `ImageReader::open(out).with_guessed_format()` reports the target format (magic bytes);
  - the dimensions are equal;
  - **the source's bytes and mtime are unchanged** (compare its length and a hash of its full contents);
  - no `.grape-tmp-` file remains.
- Late collision: after planning, create the planned name. `run_job` writes `name 2.ext`, sets `renamed = true`, and the pre-existing file's bytes are unchanged.
- EXIF orientation: a 40×20 JPEG with an Orientation=6 EXIF (`set_exif_metadata`), converted to PNG, gives a 20×40 output.
- JPEG output from RGBA: no error, white where alpha = 0.
- Corrupt input (random bytes named `.png`): `DecodeFailed`, no temp left, source unchanged.
- Unsupported (`png → avif`, `png → pdf`): `NotYetSupported`, nothing written.
- Cancelled before the run: `Cancelled`, nothing written.
- `supports()` truth table: exactly 20 true pairs among `pairs(Features::V1_ONLY)`.

Run: `cargo test -p grape-engines`, `cargo clippy -p grape-engines --all-targets -- -D warnings`. Engines tests run on Linux **and** in Windows CI (the hard-link path is exercised on NTFS there).

---

## F. App agent: glue in `apps/desktop/src-tauri`

Modules: `controller.rs`, `coords.rs`, `keys.rs` (KeyMap, edge_mode), `events.rs` (pure, tested); `env_fs.rs` (Env impl: `classify` via `fs::metadata`); `pipeline.rs` (threads + effect interpreter); `jobs.rs` (runner + toasts); `commands.rs`; changes to `lib.rs`, `tray.rs`, `ui_windows.rs`, `tauri.conf.json`, `capabilities/default.json`, `Cargo.toml`.

### F.1 Cargo.toml
- Add `grape-core.workspace = true`, `grape-engines.workspace = true`, `serde.workspace = true`, and `serde_json.workspace = true` (dev).
- Features: `default = ["overlay-subclass", "drag-monitor"]`, plus `drag-monitor = []`. Without it Grape is tray-only; the Refuse drop targets on settings and toast are **always** installed.

### F.2 tauri.conf.json
- Constant browser args on **every** window (and the settings builder `.additional_browser_args(BROWSER_ARGS)`): `"--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required"`.
- `overlay`: unchanged except `additionalBrowserArgs`. Keep `dragDropEnabled: true`, because that's what makes wry call `SetAllowExternalDrop(false)` before we replace its targets.
- New `toast` window: `url: "toast.html"`, 320×64, `visible: false`, `transparent`, `decorations: false`, `alwaysOnTop`, `skipTaskbar`, `focus: false`, `focusable: false`, `shadow: false`, `resizable: false`, `dragDropEnabled: true` (same reason), same `additionalBrowserArgs`.
- `capabilities/default.json`: `"windows": ["overlay", "settings", "toast"]`; the permissions stay `core:event:allow-listen` and `allow-unlisten`.

### F.3 Startup (`setup`, main thread)
1. Existing tray and `prepare_overlay`. Harden the **toast** with the same `harden_overlay(hwnd)`.
2. Windows: `install_drop_target(overlay_hwnd, DropPolicy::Accept(drop_tx))` and `install_drop_target(toast_hwnd, DropPolicy::Refuse)`. Log the counts. **If the overlay install fails, don't start the drag pipeline** (log an error and run tray-only), so wry's flawed target never sees a wheel drop.
3. Settings window: after `build()`, `run_on_main_thread(install_drop_target(settings_hwnd, Refuse))`.
4. Feature `drag-monitor` on Windows: `MonitorConfig { any_app: env GRAPE_ANY_APP == "1" }`, then `drag_monitor_with(cfg)?.start(drag_tx)`. Keep the monitor in `GrapeState` (`Mutex<Option<Box<dyn DragMonitor>>>`) for pause and resume. On other OSes, log "drag monitor unsupported on this OS" and skip.
5. Log level: `GRAPE_LOG=debug` gives `LevelFilter::Debug` (positions on every Over). Otherwise Info.

### F.4 Threads
- Channels: `sync_channel::<DragSignal>(256)` and `sync_channel::<DropSignal>(256)`. Two tiny forwarder threads map both into one `std::sync::mpsc::Sender<ControlMsg>`. Tray pause and resume send `Input::SetPaused` on the same channel.
- Controller thread `grape-wheel`:
  ```
  loop {
      let timeout = ctrl.next_deadline().map(|d| d.saturating_duration_since(now));
      match rx.recv_timeout(timeout or 1 s) { msg → handle(now, input), Timeout → handle(now, Tick) }
      for e in effects { interpret(e) }
  }
  ```
  Log every state change: `wheel: Dragging -> WheelOpen(conversions) seq=3 anchor=(812,433)phys/(541.3,288.7)log monitor=… scale=1.5`.
- `interpret`:
  - `PrepareOverlay{m}`: `run_on_main_thread`, `set_position(Physical(m.x, m.y))`, `set_size(Physical(m.w, m.h))`, `set_position` again. Read back `outer_position` and `inner_size`; if they differ from `m`, apply once more and log at warn.
  - `ShowOverlay` and `HideOverlay`: `run_on_main_thread(show / hide)`. **Never** `set_focus` or `set_ignore_cursor_events`.
  - `Emit(e)`: `app.emit_to("overlay", name, payload)`.
  - `Run(r)`: `jobs::submit(app, r)`.
- Latency log: record the `Instant` of each open edge by seq. `wheel_painted(seq)` logs `+N ms`.

### F.5 Jobs (`jobs.rs`)
`submit(app, RunRequest)`:
- `entry` is an `Action` → info toast "File actions arrive in a later build." `Tool` → info toast "Tools arrive in a later build."
- `Convert{to}`: if any item fails `grape_engines::supports(&op, item.format())` → info toast "Not in this build yet / {FROM} to {TO} is coming…". Use the first unsupported item's format.
- Otherwise `spawn_blocking`:
  1. Build the `Selection`: for each distinct parent folder, plus the fallback `app.path().download_dir()?.join("Grape")`, build a `DirSnapshot{ names: read_dir file names (lossy), writable: probe }`.
     - Probe: `OpenOptions::create_new` on `.grape-probe-{pid}-{nanos}` then `remove_file`.
     - A missing fallback dir counts as `writable = true` if its parent exists; it's created with `create_dir_all` only when a job writes into it (`used_fallback`, or an output setting that names it; the app passes it as `EngineCtx::fallback_dir`). Any other missing destination is a `WriteFailed`.
  2. Plan with `grape_core::plan(&sel, &op, &Settings::new(fallback))`. A `PlanError` gives an error toast whose message is the error's `Display`.
  3. Toast `working` `Making {display_path file name}…` (id = batch id, an `AtomicU64`). Position the toast window on `r.display`: centered at `anchor_phys.x`, top = `anchor_phys.y + (BACKDROP_R + 16)·scale`, clamped into the work area. `set_position`, `set_size(320×64 logical × scale)`, `show`.
  4. Run the jobs sequentially with an `EngineCtx` whose `fallback_dir` is the settings' fallback. Report progress per job as `(done + fraction) / n`, throttled to 10 Hz.
  5. Finish: `done`, `error` or partial. `revealable` = at least one output exists. Remember `outputs[0]` for that id. Hide after 3 s (done or info) or 6 s (error) unless a newer id has been shown since.
- `toast_clicked(id)`: if it's revealable, run `explorer.exe` with `raw_arg(format!("/select,\"{}\"", path.display()))` (Windows only, via `std::os::windows::process::CommandExt`). Elsewhere, log it.
- Concurrency: one blocking task per drop. That's fine for this slice; queueing comes later.

### F.6 Settings defaults in this slice
`KeyMap` Shift/Ctrl/Alt, `Features::V1_ONLY`, sounds on, fallback `{Downloads}\Grape`. There's no persistence yet (tauri-plugin-store comes with M6).

### F.7 Tests
The pure modules get unit tests (§B.4, coords, golden JSON for events, `edge_mode` truth table). Glue is verified on the PC. Run `cargo test -p grape`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo fmt --all`.

---

## G. Feature flags and diagnostics (per CLAUDE.md "Don't guess platform behavior")
- `drag-monitor` (app, on by default) is the whole drag pipeline. Build with `--no-default-features --features overlay-subclass` to turn it off.
- `overlay-subclass` (existing, D11).
- `GRAPE_ANY_APP=1` arms for drags from any app (an O5 override until Settings exists).
- `GRAPE_LOG=debug` logs every Over and hook move position.
- The log (`%LOCALAPPDATA%\io.github.sussyswimmer.grape\logs`) contains, per drag:
  - drag start (position, root class, shell drag-image flag);
  - every modifier change and edge;
  - open with anchor (physical and logical), monitor, scale and clamp result;
  - the overlay's placed rect as read back;
  - Enter paths and position;
  - highlight changes with `(dx,dy)` and the hit;
  - Drop paths, position, hit and decision (run or cancel);
  - close reason; the painted latency;
  - job plan (output paths), outcome and timing.
- Never log file contents.

---

## H. Docs (orchestrator, after merge)
- **DECISIONS** (all "proposed, confirm"). The numbers below were this plan's working numbers. What landed in `DECISIONS.md` is D13–D17: the image crate and the no-clobber finalize (D18 and D19 here) are both in D16, and `hit_test` is D17. D18 onward in `DECISIONS.md` are later, unrelated decisions.
  - **D13 Own drop target on every Grape window.** The evidence is §1.1. The overlay accepts as COPY, always written, including in `Drop`. Settings and toast refuse with NONE. wry still gets `dragDropEnabled: true` so it disables WebView2's external drop. Revisit if wry starts writing the effect in `Drop`.
  - **D14 Esc on Windows.** It closes the wheel, and the OS cancels the drag (standard OLE). Amend INTERACTION_SPEC §6 / ROADMAP M1 / TESTING. Optional later: a "dismiss but keep dragging" gesture.
  - **D15 Overlay hit-testing.** The overlay is non-layered (blur-behind); never set ignore-cursor-events; a near-transparent hit fill. Confirm on the PC.
  - **D16 Toast and event contract.**
    - The toast is a separate `toast` tool window (topmost, non-activating, drop-refusing, click to reveal).
    - Rust composes the toast copy.
    - Events are `wheel://open|highlight|close` and `toast://show|hide`; they replace `job://progress|done` in ARCHITECTURE and the UI CLAUDE.md.
    - New app commands: `wheel_painted` and `toast_clicked`.
    - Controller timings: 300 ms no-paths (capped at 1.5 s), a 750 ms drop grace after `Ended`, the 30 s watchdog, and `Running` collapsed into a worker.
  - **D17 O5 resolved (proposed): the default allowlist is Explorer and Desktop** (`CabinetWClass, ExploreWClass, Progman, WorkerW` via `GA_ROOT`), never Grape's own windows. `GRAPE_ANY_APP=1` overrides it until Settings has an "Any app" toggle.
  - **D18 image 0.25.10** (MIT OR Apache-2.0), features jpeg/png/webp/bmp/tiff, plus the transitive list and licenses from the engines agent's summary (moxcms and pxfm are BSD-3-Clause OR Apache-2.0; tiff is MIT). WebP output is lossless only; AVIF, HEIC, SVG and PDF come later.
  - **D19 No-clobber finalize.** `hard_link`, falling back to a `create_new` copy, with no `unsafe` and no new dependencies, so ARCHITECTURE's `MoveFileExW` / `renamex_np` line gets updated.
  - **D20 npm `@tauri-apps/api` 2.12.1** (Apache-2.0 OR MIT); the shared `additionalBrowserArgs` with autoplay on all windows.
  - **D21 The `drag-monitor` feature and `GRAPE_LOG`.**
  - **D22 Key-down edge only.** Modifiers held when a drag starts are a baseline and never open the wheel (protects Explorer's Shift-move and Ctrl-copy drags). Remove O5 from the open questions.
- **ARCHITECTURE:**
  - `DragSignal::Escape`, `DropSignal`, `MonitorConfig`, `install_drop_target`.
  - The drag-drop bullets become "own `IDropTarget`, global physical coordinates" instead of Tauri's `DragDropEvent`.
  - The event names, the toast window, and the engines `run_job`/`GrapeError` signature.
- **INTERACTION_SPEC §6 and §1:** Esc note for Windows (D14); the drop grace; the baseline modifiers.
- **PLATFORM_WINDOWS:** "The drop must never move the source" now records D13's finding and the scripted CI test; add the hook health check and the button-swap note.
- **ROADMAP:** tick only what CI and the container prove. In M1, the Windows `DragMonitor` item can be ticked as "built + unit/CI-tested" with "(check on the PC)", like M0's wording. Add a status line. In M3, add sub-bullets: image raster engine (5 formats) done; toast done; atomic writes done.
- **TESTING:** a new section "Wheel slice (M1 + M3a) on the PC" (§J list); change the Esc line per D14; add `pnpm tauri dev` steps and the log location.
- **README "What you can test now"**: drag a JPG/PNG/WEBP/TIFF/BMP from Explorer, tap Shift, drop on a format, and a new file appears next to it. Other segments show "not in this build yet". Pause from the tray. Mention `GRAPE_ANY_APP=1` and where the log is.

---

## J. Risks, verification here, and the manual QA list for Maxwell

### Risks (most serious first)
1. **WebView2 child hit-testing or OLE re-targeting doesn't reach our target.** Mitigations: the hit fill and registration on every in-process descendant (wry's proven pattern). Detected by "no Enter" in the logs; the 300 ms no-paths close keeps rule 4.
2. **The Drop/Ended race or Explorer timing** differs from Wine's 50 ms tracker. The 750 ms grace and the log timings cover it; tune on the PC.
3. **Rubber-band selection with Shift or Ctrl in Explorer** arms like a drag, so the neutral ring flashes for up to 300 ms. Mitigation candidate: require `shell_drag_image_visible()` (logged now and decided after the PC test).
4. **Mixed DPI placement:** `WM_DPICHANGED` resizing; the set_position, set_size, set_position sequence plus a read-back check.
5. **Hook removal under load** (`LowLevelHooksTimeout`): the health check reinstalls it; the left-button poll synthesizes `Ended`.
6. **Autoplay blocked:** the tick is silent, which isn't critical; QA item.
7. **Large monitors:** a full-screen transparent webview may cost frames, so measure latency (target < 100 ms) with `wheel_painted`.
8. ~~**Corner hits:** Rust's hit test includes the small rounded-off outer corner areas (≤ 20 px), so a drop there counts as a segment.~~ Resolved: `grape_core::hit_test` now honors the fillets (`fillet_radius(n)`, the same rule as `filletRadius`) and treats one item as a full ring with no gap (`INTERACTION_SPEC.md` §4), so no UI `isPointInFill` re-check or reverse channel is needed.

### Verifiable in this Linux container
- `cargo test --workspace` (grape-core; platform detector and mock; controller, coords, events and edge_mode; engines' full 20-pair conversions with source-hash and no-temp assertions).
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check`.
- `cargo clippy -p grape-platform --all-targets --target x86_64-pc-windows-msvc -- -D warnings`. Try the same for `-p grape-engines`; for the app on the Windows target, rely on CI if Tauri's Windows build scripts can't run on Linux.
- `pnpm -C apps/desktop/ui typecheck && pnpm -C apps/desktop/ui test && pnpm -C apps/desktop/ui build`.
- **Windows CI** (`windows-latest`): everything above runs natively, including the platform Win32 tests and **the scripted golden-rule-2 drop-effect test** (§A.3, test 3).
- No Xvfb smoke run: the drag monitor is unsupported on Linux, and a Linux Tauri run would prove nothing about the Windows behaviors at risk.

### Manual QA on the PC (add to TESTING.md; run `pnpm tauri dev` from `apps/desktop` and keep the log open)
1. **Golden rule 2, first:** on a scratch copy, Shift-drag a JPG from Explorer onto the open wheel 10×. Drop on a segment 5× and off-ring 5×. The source is still there every time (also from a second drive and from the Desktop). Repeat with Ctrl and Alt.
2. Drag a PNG in Explorer and tap Shift: the wheel appears at the cursor within about 100 ms (check the `painted +N ms` log line). It is clamped near screen edges and the taskbar.
3. Hover segments: highlight, center label and tick. Leaving the ring clears the label.
4. Drop on JPG: a toast reads "Making photo.jpg…" and then "Done". `photo.jpg` appears next to the source; if one exists, `photo 2.jpg`. Click the toast and Explorer selects the file.
5. Drop on HEIC, AVIF or PDF: the info toast "Not in this build yet". Nothing is written.
6. Drop in the dead zone, in a gap, and on empty screen over another app (e.g. Notepad): nothing happens, nothing lands in that app, the source is unchanged (this checks the hit fill).
7. Shift, then Ctrl mid-drag: the wheel swaps to tools in place. Alt swaps to actions. Shift+Ctrl together: no change.
8. Tap Shift and release it, keep dragging: the wheel stays open.
9. **Hold Shift before starting the drag** (Explorer move): no wheel; Explorer moves the file as usual.
10. Esc with the wheel open: the wheel closes and Explorer cancels the drag (D14). The file stays put.
11. Drag selected text from a browser (with `GRAPE_ANY_APP=1`) and tap Shift: the wheel closes by itself, and the text drop still works where you release it.
12. Drag a folder and tap Shift: an empty-state message naming Alt. Tap Alt: six actions. Dropping on an action shows "File actions arrive in a later build."
13. Multi-select 3 PNGs, Shift, drop on JPG: three outputs and one toast "Making 3 files…" then "Done".
14. Second monitor at a different scale (if available): the wheel is correctly placed and hit-tested on both. The log's placed rect equals the monitor rect.
15. Drag from an elevated Explorer: either no wheel, or a neutral ring that closes within about 0.3 s, and the drop isn't hijacked.
16. Pause Grape from the tray: drags behave normally, with no wheel. Resume: it works again.
17. The overlay and toast never take focus (keep typing in Notepad while a toast shows), and never appear in the taskbar or Alt-Tab.
18. A read-only folder (e.g. `C:\Program Files` sample): the output goes to `Downloads\Grape` and the toast says so.
19. Kill Grape mid-drag in Task Manager: the overlay disappears and the drag completes normally.
20. A Shift+drag rubber-band selection in an empty Explorer area: note whether the neutral ring flashes. That decides risk 3.
