# Decisions log

Append-only. Each entry records the decision, why it was made, and what would change it. Open questions live at the bottom until they're decided.

## D1: Tauri 2, single codebase (2026-10-19, decided by Maxwell)
- **Decision:** one Tauri 2 app for macOS and Windows. Shared Rust core and TS wheel UI, with thin per-OS modules.
- **Why:** one conversion engine and one wheel implementation, split cleanly between Claude Code and Codex lanes.
- **Revisit if:** the overlay can't float or receive drops reliably on one OS after the M1 spike. Fallback: a native overlay (AppKit `NSPanel` / Win32 layered window) hosted by the same Rust app.

## D2: Grape is free and open source (2026-10-19, decided by Maxwell; open-source part assumed — confirm)
- **Decision:** free, no paid tier, no conversion quota. Source public on GitHub. App code license: **MIT (assumed default, confirm)**.
- **Consequence for codecs:** GPL tools (FFmpeg with x264, x265/`heif-enc`, pandoc) may ship as **separate sidecar executables** with license texts and source links. Never link GPL code into Grape's own binaries. See `ENGINES.md`.
- **Revisit if:** Grape ever goes paid or closed-source. Then re-audit every sidecar and drop GPL builds for LGPL builds plus hardware encoders.

## D3: Clean-room rule
- **Decision:** Tangerine's public website demo was analyzed with REA for **behavior only**. No Tangerine code, assets, copy, sounds or branding go into Grape. The Tangerine app binary is **not** to be decompiled: its App Store license restricts reverse engineering, and Grape doesn't need it.
- **Why:** legal safety and originality. The behavior spec in `docs/` is enough.

## D4: Direct distribution before stores
- **Decision:** v1 ships as a notarized DMG (macOS) and NSIS installer (Windows) on GitHub Releases. Mac App Store and Microsoft Store are later.
- **Why:** the Mac sandbox complicates writing outputs next to sources. Store review is slow for a fast-moving v1.
- **Note:** Developer ID signing and notarization need an Apple Developer Program membership ($99/yr). The account holder must be of legal age, so a parent or guardian can hold it. Without it, users must approve the app in System Settings → Privacy & Security on first launch. The README should explain this if we ship unsigned.

## D5: Outputs next to the source, never overwrite
- **Decision:** default output location is the source's folder, with `" 2"` collision naming. Fall back to `~/Downloads/Grape` if the folder isn't writable.
- **Why:** observed in Tangerine's naming and matches user expectation.

## D6: Windows default keys Shift / Ctrl / Alt
- **Why:** the Windows key opens Start and can't be used mid-drag. Remappable.

## D7: No Accessibility/Input Monitoring permission on macOS
- **Decision:** detect drags via the drag pasteboard and poll modifiers and mouse state. Never request Accessibility.
- **Revisit if:** polling proves unreliable in M1. Then ask for the permission with a clear onboarding explanation.

## D8: Windows first, macOS second (2026-10-19, decided by Maxwell)
- **Decision:** Phase 1 builds and ships Grape for Windows. The macOS port (Phase 2, ROADMAP M8–M9) starts after Windows v1 ships.
- **How it's built:** Maxwell builds with Claude Code on his own Windows PC, which is also the test machine for every manual QA item.
- **Consequence:** until M8, `grape-platform` keeps a stub `macos` module that compiles but does nothing. Don't spend time on AppKit code in Phase 1. Shared code (`grape-core`, `grape-engines`, the UI) must stay OS-neutral so the port is cheap.

## D9: Toolchains, dependencies and the 14-day rule (2026-10-22, proposed, confirm)
- **Decision:** pin the toolchains, and only adopt releases that are at least 14 days old when they're added. Security fixes override the 14-day rule.
  - **Rust 1.99** via `rust-toolchain.toml` (rustfmt, clippy, minimal profile). It resolves to 1.99.1. CI uses the same file, so clippy lints match here, in CI and on the PC.
  - **pnpm 10.28.0** via `packageManager`, and Node ≥ 22.12. `minimumReleaseAge: 20160` in `pnpm-workspace.yaml` enforces the 14 days for every npm package, transitive ones included.
  - **Cargo** has no such setting. Direct dependencies use exact `=` pins where a family must move together (tauri, tauri-build and the CLI), and `Cargo.lock` was checked crate by crate against crates.io publish dates. The Tauri family is locked to the 2.12.1 release set: tauri-runtime 2.12.1, tauri-runtime-wry 2.12.1, tauri-utils 2.10.1, tauri-codegen 2.7.1, tauri-macros 2.7.1, tauri-plugin 2.7.1, tauri-winres 0.3.6. Underneath are tao 0.37.1, wry 0.57.0, muda 0.20.0, tray-icon 0.25.1 and webview2-com 0.39.1.
- **Rust crates:**

| Crate | Version | License | Why |
|---|---|---|---|
| tauri | =2.12.1, features `tray-icon`, `macos-private-api` | Apache-2.0 OR MIT | App shell (D1). `macos-private-api` must match `macOSPrivateApi` in `tauri.conf.json` (D11) |
| tauri-build (build) | =2.7.1 | Apache-2.0 OR MIT | Validates config and capabilities; compiles Windows resources |
| tauri-plugin-single-instance | =2.5.2 | Apache-2.0 OR MIT | A second launch opens Settings instead of adding a second tray icon |
| tauri-plugin-log | =2.10.0 | Apache-2.0 OR MIT | Logs to stdout and the app log folder, rotated (5 × 1 MB) |
| anyhow | 1.0.104 | MIT OR Apache-2.0 | Errors in the app crate only |
| log | 0.4.34 | MIT OR Apache-2.0 | Logging facade |
| windows (Windows only) | 0.62.2 | MIT OR Apache-2.0 | Win32 calls in `grape-platform`; same major as Tauri 2.12, so the build is shared |
| serde | 1.0.229 | MIT OR Apache-2.0 | `grape-core` types the UI receives (D12) |
| thiserror | 2.0.21 | MIT OR Apache-2.0 | Library errors (`PlanError`, `PlatformError`) |
| proptest (dev) | 1.11.0 | MIT OR Apache-2.0 | Hit-test property tests |
| serde_json (dev) | 1.0.151 | MIT OR Apache-2.0 | Serde shape tests and doc-sync parsing in `grape-core` |

- **npm packages** (dev and build only; only the built UI files ship):

| Package | Version | License | Why |
|---|---|---|---|
| @tauri-apps/cli | 2.12.1 (exact; moves with tauri and tauri-build) | Apache-2.0 OR MIT | The `tauri` CLI |
| vite | ^8.3.3 → 8.3.3 | MIT | Dev server and build |
| typescript | 7.0.2 | Apache-2.0 | `tsc --noEmit` (the native 7.x compiler; 6.0.3 is the fallback if a tool needs the JS API) |
| vitest | ^5.0.3 → 5.0.3 | MIT | UI tests |
| @types/node | 22.20.5 | MIT | Type-checks `vite.config.ts` (pulls in undici-types 6.21.0, MIT) |

- **Licenses:** Tauri statically links `option-ext` 0.2.0 (MPL-2.0) via `dirs` → `dirs-sys`, which ENGINES rule 1 doesn't list. See **O6**. Vite's lightningcss (MPL-2.0) and the cssparser/selectors/dtoa-short crates (MPL-2.0) only run at build time and aren't shipped. Everything else is MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, Unlicense, CC0 or 0BSD.
- **Revisit if:** a pinned crate gets a security fix (take it right away), or Tauri 2.12.x patches pass 14 days (bump tauri, tauri-build and the CLI together).

## D10: App identifier `io.github.sussyswimmer.grape` (2026-10-22, proposed, confirm)
- **Decision:** the Tauri `identifier` (bundle id) is `io.github.sussyswimmer.grape`. The bundle `publisher` is `sussyswimmer` and the copyright matches `LICENSE`.
- **Why:** reverse DNS needs a domain the project controls, and `sussyswimmer.github.io` is free and already ours. Without `publisher`, Tauri would name "github" as the publisher of the exe and installer.
- **Consequence:** hard to change later. It names the app-data and log folders (`%APPDATA%\io.github.sussyswimmer.grape`, `%LOCALAPPDATA%\io.github.sussyswimmer.grape\logs`), the single-instance lock, the settings store and the macOS bundle id. Changing it orphans users' settings.
- **Revisit if:** the repo moves to an organization or gets its own domain. Decide before the first public release (M7).

## D11: Overlay window flags on Windows (2026-10-22, proposed, confirm)
- **Decision:** the overlay is declared in `tauri.conf.json` with label `overlay`, `visible: false`, `transparent`, `decorations: false`, `alwaysOnTop`, `skipTaskbar`, `focus: false`, `focusable: false`, `shadow: false`, `visibleOnAllWorkspaces`, `resizable: false`, `dragDropEnabled`.
  - `focusable: false` is what sets `WS_EX_NOACTIVATE` (tao 0.37.1 maps it); `focus: false` only controls initial focus. `alwaysOnTop` sets `WS_EX_TOPMOST`. `shadow: false` avoids a 1 px white border on an undecorated window. `visibleOnAllWorkspaces` is a no-op on Windows and kept for macOS.
- **`WS_EX_TOOLWINDOW` needs a subclass.** It has no config key, and tao rewrites the whole extended style on every window-flag change, including each show and hide, so a one-time `SetWindowLongPtrW` would be lost. `grape_platform::windows::harden_overlay`, called from `setup` on the thread that owns the window:
  - installs a `SetWindowSubclass` proc that forces `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` on and `WS_EX_APPWINDOW` off in every `WM_STYLECHANGING` for `GWL_EXSTYLE`, and adds `SWP_NOACTIVATE` to every `WM_WINDOWPOSCHANGING` (tao shows windows with `SW_SHOW`, which would otherwise activate them). It removes itself on `WM_NCDESTROY`.
  - applies the styles once with `SetWindowPos(HWND_TOPMOST, NOMOVE | NOSIZE | NOACTIVATE | FRAMECHANGED)`, and refuses windows owned by another thread.
  - A failure is logged and doesn't stop startup.
- **Feature flag:** this sits behind the app crate's Cargo feature `overlay-subclass`, on by default. Build with `--no-default-features` to compare without it.
- **Checking it:** at startup the app logs the real ex-style. Debug builds add the tray item "Debug: flash overlay", which shows the overlay for 2 s to check focus, taskbar and Alt-Tab by hand (`TESTING.md` → App shell). Explicit `set_focus` / `SetForegroundWindow` calls still activate the window, so the controller must never make them.
- **`macOSPrivateApi: true`** stays, as the spec asks, but Tauri's 2.12.1 schema says it's a no-op now that the APIs are always enabled. tauri-build still requires the `macos-private-api` Cargo feature to match it.
- **Tested:** compiled for `x86_64-pc-windows-msvc`; the Windows unit tests (real HWND, style rewrite, cross-thread refusal) run in CI. Not yet run on the PC.
- **Revisit if:** the PC shows focus theft or a taskbar or Alt-Tab entry (turn the feature off to compare), or Tauri/tao gains a native tool-window option.

## D12: grape-core contract extensions (2026-10-22, proposed, confirm)
- **Decision:** keep the seven signatures in `crates/grape-core/CLAUDE.md` exactly, and add:
  - **Tiers:** `Tier { V1, V2, Later }`, `Features { v2, later }` (`V1_ONLY` is the default; `FULL`), `pair_tier`, `targets_enabled`, `pairs(features)`, `tool_tier`, `tools_enabled`, and `TIER_OVERRIDES` (compress on pdf is v2). The full tables stay in the data and are filtered at the edge.
  - **`wheel_content(items, mode, features) -> WheelContent`:** entries or an empty state. Conversions are the intersection of each format's enabled targets, in the first item's order. Tools are the intersection of each format's enabled tools, so a tool must be enabled for every format (in v1, jpg + pdf drops compress). Folders and unknown formats get only actions, even with v2 on, because folder archives aren't specified yet (`INTERACTION_SPEC.md` §7). Actions always has 6.
  - **`Platform { Windows, MacOs }`:** titles come from `ActionKey::title(Platform)` and `WheelEntry::title(Platform)`, so core has no `cfg`.
  - **Geometry:** `GEOMETRY` (the §4 constants, serializable for the UI), `clamp_anchor(anchor, work_area)` with margin `BACKDROP_R + 14 = 140` in logical units, and `Point` / `Rect`.
  - **Outputs:** `JobOutput { File, Folder, PagesFileOrFolder { file, folder, page_stem } }` and the page helpers `produces_pages` (any document → jpg/png), `pages_folder_name` and `page_file_name`.
  - **Selection:** `Selection { items, dirs }` carries a `DirSnapshot { names, writable }` for each folder a plan may write to (source folders, the output folder, the fallback). This keeps `plan` at three arguments and free of I/O. A missing snapshot is an error (`MissingDirSnapshot`), never "assume writable". The selected sources' own names always count as taken. Items may carry a sniffed `Format`, and planning uses it (compress on WAV content → `.flac` whatever the extension says).
  - **Tool parameters:** `ToolParams { compress, channels, speed, target_lufs }` inside `Operation::Tool`, defaults Balanced / Mono / 2x / −16 LUFS. `Speed` is whole hundredths, so it's exact and hashable.
  - **Errors:** `PlanError { EmptySelection, NotOffered, NoFileName, NoParentDir, MissingDirSnapshot, NoWritableDestination }` (thiserror).
- **Collisions:** names are compared case-insensitively with full Unicode case folding (ẞ, ß and SS collide), which over-matches NTFS and APFS on purpose. Unicode normalization (NFC vs NFD) is **not** folded, so engines must never replace an existing file when they move an output to its final name (`ARCHITECTURE.md` → grape-engines). That no-clobber rename is the last guard for golden rule 2.
- **Dependencies:** serde, thiserror; dev-only proptest, serde_json (D9). `strum` is allowed but not used.
- **Why:** lane A gets tier filtering, wheel contents, titles and geometry from core instead of re-deriving the rules in the UI, and the directory snapshot keeps core pure and deterministic. M2 was built by Claude Code at Maxwell's request, although ROADMAP gives `grape-core` to lane B.
- **Revisit if:** planning needs more disk state than names and writability (e.g. free space), or lane A would rather pass `Features` straight into `targets` / `tools`.

## D13: Grape's own drop target on the Windows overlay (2026-10-22, proposed, confirm)
- **Decision:** Grape registers its own `IDropTarget` (`grape_platform::windows::install_drop_target`) on the overlay window and every child window, replacing wry's, at startup and again before each show. It reports `DROPEFFECT_COPY` for a file drag and `DROPEFFECT_NONE` for anything else, in `DragEnter`, `DragOver` and `Drop`, whatever keys are held and whatever the source allows. On Windows the controller gets its drop events (`grape_platform::DropEvent`, global physical pixels) from this target instead of Tauri's `WindowEvent::DragDrop`.
- **Why:** wry 0.57.0 (`webview2/drag_drop.rs`) sets the effect in `DragEnter`/`DragOver` but never writes it in `Drop`, and leaves it unset for drags without files. What Explorer then acts on is whatever value OLE left there, and golden rule 2 can't rest on that (`PLATFORM_WINDOWS.md` → "The drop must never move the source"). The target's screen coordinates are also what the hit test wants.
- **Kept:** `dragDropEnabled: true` in `tauri.conf.json`, so wry still turns off WebView2's own external drops (`SetAllowExternalDrop(false)`). A window Grape misses then refuses the drop instead of navigating. If wry's handler ever fires, the app logs it as a coverage gap.
- **Dependencies:** `windows` 0.62.2 gains the features `Win32_Graphics_Gdi`, `Win32_System_Com`, `Win32_System_Com_StructuredStorage`, `Win32_System_LibraryLoader`, `Win32_System_Ole`, `Win32_System_SystemServices`, `Win32_UI_HiDpi` and `Win32_UI_Input_KeyboardAndMouse`, all of which wry already enables on the same version. `windows-core` 0.62.2 (MIT OR Apache-2.0) becomes a direct dependency of `grape-platform` because the COM `#[implement]` macro needs it by name; it was already in the build through wry and tao.
- **Tested:** CI's Windows runner drives the target with the shell's own data object for a temporary file, with Shift, Ctrl, Alt and Shift+Ctrl held and every effect allowed. It always answers copy, and nothing for a drag without files. Not yet run against a real Explorer drag (`TESTING.md` → "Drag + wheel").
- **Revisit if:** the PC shows drops that don't reach Grape's target (a WebView2 child window owned by another process can't be registered; the log says how many were), or wry starts setting the effect in `Drop`.

## D14: Drag sources and keys on Windows (2026-10-22, proposed, confirm)
- **Drag sources:** a press arms the wheel only if it lands in a top-level window of class `CabinetWClass` / `ExploreWClass` (File Explorer) or `Progman` / `WorkerW` (the Desktop), as `PLATFORM_WINDOWS.md` describes. This answers O5 with its default. An "Any app" setting can come with Settings in M6.
- **Keys held before the drag don't count:** `DragSignal::Started` carries the modifiers already held, and only a change during the drag opens or swaps a wheel. Holding Shift before dragging keeps meaning "move" in Explorer.
- **Exactly one key:** a wheel opens only when exactly one modifier is held and it's assigned to a wheel. The Windows key is tracked, so it counts towards "two keys", but opens nothing.
- **Esc** is polled with `GetAsyncKeyState`, like the modifiers. An OLE drag source such as Explorer also cancels its own drag when Esc is pressed, so on Windows Esc ends the drag as well as hiding the wheel (`INTERACTION_SPEC.md` §6). To be confirmed on the PC.
- **Revisit if:** users want the wheel from other apps (browsers, editors), or the held-key rule feels wrong in testing.

## D15: Overlay ↔ Rust messages (2026-10-22, proposed, confirm)
- **`@tauri-apps/api` 2.12.1** (Apache-2.0 OR MIT), pinned to match `tauri` 2.12.1 and over 14 days old (D9). The overlay uses it to listen for `wheel://open`, `wheel://highlight` and `wheel://close`, and to call one command.
- **`wheel_painted(seq)`:** the overlay reports when a wheel is on screen, so the log shows how long it took from the key (ROADMAP M1: under 100 ms). `build.rs` lists the app's commands, so each gets its own permission, and `capabilities/overlay.json` grants only `allow-wheel-painted`, only to the overlay. Unlisted commands can't be called.
- **App crate dependencies:** `grape-core` (wheel layout, hit test, anchor clamping) and `serde` (event payloads), both already in the workspace (D9).

---

## Open questions

- **O1: Confirm the open-source license.** MIT is assumed. GPL-3.0 is the alternative if you want forks to stay open. Note: the repo's `LICENSE` file is already MIT ("Copyright (c) 2026 sussyswimmer"), and `Cargo.toml` and the bundle say MIT.
- **O2: Apple Pages support.** There's no open library and `NSAttributedString` can't read `.pages`. How Tangerine does it is unknown; the website doesn't reveal it. Options: (a) skip; (b) macOS-only via scripting the Pages app if installed; (c) parse the IWA format (large effort). Default: skip until users ask.
- ~~**O3: Windows test hardware.**~~ Resolved: Maxwell has his own Windows PC (see D8).
- **O4: Optional add-on downloads** (LibreOffice for slides/office in v2). This needs a network call outside the conversion path. Decide before v2.
- ~~**O5: Show the wheel for drags from any app on Windows,** or only Explorer and the Desktop (the default)?~~ Answered by default in D14: Explorer and the Desktop only, with "Any app" as a possible setting in M6. Confirm or change.
- **O6: MPL-2.0 crates linked by Tauri.** Tauri 2.12 statically links `option-ext` 0.2.0 (MPL-2.0) through `dirs` → `dirs-sys`. ENGINES rule 1 allows only MIT/Apache/BSD/ISC/Zlib, or LGPL linked dynamically. MPL-2.0 is file-level copyleft: linking it into an MIT app is fine as long as the MPL files' source stays available, which it does (unmodified on crates.io). Proposal: amend rule 1 to allow unmodified MPL-2.0 crates, list them in `THIRD_PARTY_NOTICES.md` in M7, and add a `cargo-deny` license check to CI.
- **O7: Wheel titles.** `TOOLS_AND_ACTIONS.md` lists the wheel titles (METADATA, ADD BG, BLEEP, …) as **[observed]** from the demo, while golden rule 1 bans Tangerine's wording. `grape-core` encodes them exactly as the doc lists them, in one `match` checked by the doc-sync test, so rewording them later is a one-line change on each side. Keep them, or write our own?
