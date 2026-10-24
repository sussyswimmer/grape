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

- **`windows` features** grow with the milestones, never as new crates. D13 lists M1's. The M5 file actions add `Win32_System_DataExchange` and `Win32_System_Memory`, which tao already enables on the same version, and `Win32_Storage_FileSystem` (bindings only, for `GetVolumePathNameW`).

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
- **Other windows:** Settings gets the same target in refusing mode (`refuse_drops`), answering `DROPEFFECT_NONE` to everything, because tao's and wry's handlers there have the same unset effect. Every future Grape window (toast, float shelf) needs one or the other.
- **Hit-testing:** the overlay page has a fill of 1/255 black (`--overlay-hit-fill`), invisible but enough that no part of the overlay can count as see-through when Windows picks the window under a drop.
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
- **App crate dependencies:** `grape-core` (wheel layout, hit test, anchor clamping) and `serde` (event payloads), both already in the workspace (D9); `serde_json` dev-only, to check payloads against the UI's golden JSON.
- **`toast_clicked(id)`:** granted only to the toast window (`capabilities/toast.json`, with event listen/unlisten). The page sends the toast's id, never a path; Rust looks up what to reveal.
- **WebView2 flags:** every window uses the same `additionalBrowserArgs` (`ui_windows::BROWSER_ARGS`, repeated in `tauri.conf.json`; a test checks they match), because WebView2 won't start windows with different flags in one profile. Setting flags replaces wry's defaults, so the string keeps wry's `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection` and adds `--autoplay-policy=no-user-gesture-required` so the overlay's tick plays without a click.

## D16: Raster image engine and no-clobber writes (2026-10-23, proposed, confirm)
- **Engine:** `grape-engines` converts among JPG, PNG, WebP, TIFF and BMP (20 pairs) with `image` 0.25.10 (MIT OR Apache-2.0). It's crate-local, with `default-features = false` and features `jpeg`, `png`, `webp`, `bmp` and `tiff`, so there's no rayon and no other codecs.
  - JPEG is written at quality 90, with alpha flattened onto white. WebP output is lossless only, because lossy WebP would need libwebp.
  - EXIF orientation is applied to the pixels. The ICC profile is kept when the target can embed it and it matches the output's color model. A profile over 255 × 65,519 bytes (the most a JPEG holds, about 16 MiB) is left out for every target.
  - Only the first frame or page of an animated or multi-page input is converted.
  - **Memory limits**, all checked before any pixel is decoded. A job refused by one fails with `DecodeFailed` and "{name} is too large to convert.":
    - A JPEG or WebP file must be at most 512 MiB; a bigger one is refused unread. (The JPEG decoder reads the whole file into memory, and the WebP decoder reads metadata chunks whole with no limit of its own.) PNG, TIFF and BMP files can be any size: their decoders read as they go and hold their own buffers to limits, so a layered TIFF or a long multi-page scan converts its first image.
    - The PNG decoder's metadata is held by two 128 MiB limits, so it can keep up to about twice that in all:
      - Its text and EXIF chunks ahead of the image data are counted from the chunk headers before decoding: the buffer it reads chunks into (as large as the largest), about 128 bytes per text chunk plus up to twice its text (Latin-1 can double as UTF-8), and two copies of an EXIF chunk. More than 128 MiB is refused, so one text chunk of up to about 40 MiB fits, and so does a file with a few hundred thousand tiny ones.
      - The decoder's own limit holds the inflated ICC profile, the raw text and its row buffers. A profile that would inflate past it is dropped while reading.
      - The walk stops where the decoder would fail anyway (no PNG signature, a first chunk other than IHDR, a critical chunk it doesn't know, as a run of zeros reads, or a chunk running past the end of the file), so a broken file is reported as unreadable, not too large.
    - The decoded image must fit the `image` crate's 512 MiB default limit.
    - The estimated peak of the whole conversion must fit 1 GiB: what the decoder keeps, a rotated copy, the converted copy, the encoder's in-memory output and the profile carried over. What the decoder keeps is read from the file's headers first:
      - The JPEG decoder reads the file into a buffer of exactly its size and parses its headers before any check of Grape's could run. So the file is first read once, cancellably, and that bounds both:
        - Memory: the file, the decoder's copies of the metadata segments it parses (ICC, EXIF, XMP), which can't exceed the file, and the extended XMP it reassembles from those copies, at most 65,533 bytes per place the extended-XMP namespace appears and at most the file again. If that is over 1 GiB, the file is refused before decoding; otherwise it is charged.
        - Time: the header parse re-checks every pending extended-XMP segment after every marker, so a file with many of both can take hours. A job parses the headers five times, four of them inside the `image` crate from memory, where cancel can't reach. Namespace places times `FF` bytes over 2^27 is refused: about half a second per parse, three per job. Real files have no namespace places, or tens.
      - It also keeps 16-bit coefficients for every component of the whole image for a progressive frame, or a sequential one whose first scan doesn't carry every component. The frame and the first scan come from zune-jpeg's own header decode, with the options the `image` crate uses, so they can't be read differently from the decoder. (A marker walk of Grape's own could be led to a different frame, for example behind an RST segment, and that let a crafted file past the limit.) zune-jpeg doesn't report the first scan's components, but it stops right after that scan's header and requires its length to be exactly 6 + 2 × components, so the count is read back from there; unless exactly one count matches, the whole image is charged.
      - The WebP decoder keeps up to an RGBA frame for a simple file or a still lossless one; 6.5 bytes per pixel for a still lossy image with alpha (its YUV planes, the alpha chunk decoded as RGBA and the alpha plane); and an RGBA frame plus an RGBA canvas for an animation. Lossless or lossy is decided as image-webp does: by a `VP8L` chunk within the range it scans.
      - The PNG decoder keeps the metadata counted above, plus its own 128 MiB limit when there is an ICC profile to inflate.
      - JPEG and WebP headers that can't be read count as the larger case.
  - Cancel is checked while reading, converting and writing.
  - Every other target (HEIC, AVIF, SVG, PDF, audio, video) and every tool returns `NotYetSupported` with a message the toast can show.
- **API** (for the app): `supports(&Operation, Option<Format>)`; `run_job(&Job, &EngineCtx, &mut dyn FnMut(f32)) -> Result<JobOutcome { outputs, renamed }, GrapeError { kind, user_message, detail }>`, which **blocks** (run it on a worker); `EngineCtx { sidecars: SidecarPaths, cancel: CancelToken, fallback_dir: Option<PathBuf> }`, where the app sets `fallback_dir` to `Settings::fallback_dir` and the rest can stay `Default` until sidecars exist; `build_selection(&[PathBuf], &Settings) -> Selection`, which reads the disk so `grape-core` doesn't have to.
- **No-clobber writes:**
  - Output goes to `.grape-tmp-{pid}-{nanos}-{n}.{ext}` in the destination folder, created with `create_new` and synced.
  - It is then named with `std::fs::hard_link(tmp, final)`, which fails if `final` exists, so the name appears atomically with the full content. The temp is then removed.
  - If the name was taken meanwhile, the job re-plans it with `grape_core::output_name` up to 3 times (`renamed = true`), else it fails with `NameTaken`.
  - Where hard links don't work (FAT32, exFAT, some shares), it creates `final` with `create_new` and copies. That is still no-clobber but not atomic: a crash mid-copy can leave a partial file with the new name. `MoveFileExW` would be atomic but needs `unsafe`, which the crate forbids.
  - A drop guard deletes the temp on every path, retrying for up to 775 ms in case a scanner holds it. A temp that still can't be deleted stays (known limitation; a startup sweep can come later).
- **Writability probes:** `build_selection` creates and deletes a `.grape-probe-…` file in each destination folder. On Windows the probe is opened delete-on-close, so it can't be left behind. Grape's fallback folder (`Downloads\Grape`) is created only when a job writes into it, by falling back or because the output setting names that folder (the engine knows it from `EngineCtx::fallback_dir`). Until then it counts as writable if its nearest existing parent passes the probe; a link to nowhere in its place doesn't count. Any other destination folder that is gone by the time a job runs is a `WriteFailed`, never recreated.
- **Direct dependencies:** `image` and `zune-jpeg` 0.5.15, which `image` already brings in; the engine uses it to read a JPEG's headers before decoding.
- **New `Cargo.lock` entries**, all permissive and all published before the 14-day cutoff (D9): image 0.25.10, zune-jpeg 0.5.15, zune-core 0.5.3 (MIT/Apache-2.0/Zlib); image-webp 0.2.4, half 2.7.1, weezl 0.1.12 (MIT OR Apache-2.0); tiff 0.11.3, fax 0.2.7, crunchy 0.2.4 (MIT); moxcms 0.8.1, pxfm 0.1.30 (BSD-3-Clause OR Apache-2.0); byteorder-lite 0.1.0 (Unlicense OR MIT); quick-error 2.0.1 (MIT/Apache-2.0). No GPL, LGPL or MPL in the engines tree.
- **Tested:** every engine test hashes the source before and after and checks that no temp or probe file is left. CI's Windows runner exercises the NTFS hard-link path.
- **Revisit if:** lossy WebP is wanted (libwebp), or HEIC/AVIF/SVG/PDF land (ravif, resvg, pdfium).

## D17: `hit_test` follows the drawn wheel (2026-10-23, proposed, confirm)
- **Decision:** `grape_core::hit_test` selects exactly what the overlay draws (`INTERACTION_SPEC.md` §4):
  - the rounded outer corners: a pointer in a corner's cut-off, outside its fillet circle, selects nothing;
  - one item is a full ring with no gap;
  - segments too narrow to draw (from 76 items) select nothing.
- **API:** adds `pub fn fillet_radius(n) -> f64`, the corner radius `min(CORNER, the largest that fits)`, which mirrors the UI's `filletRadius`. `hit_test`'s signature is unchanged. This extends D12.
- **Why:** during an OS drag the overlay gets no pointer events, so it can't re-check `isPointInFill` as the spec's [observed] line describes. The check has to live where the hit test runs.
- **Checked:**
  - Against the UI's own `segmentPath` on a 0.5 px grid for n = 1–12: 0 disagreements in 3.15 million points away from the outlines.
  - The UI's agreement test (`ui/src/wheel/geometry.test.ts`) now covers the corners and the single-item ring, too.
- **Revisit if:** the wheel's shape changes; the UI and core must change together.

## D18: Start the macOS port now (2026-10-23, decided by Maxwell)
- **Decision:** Maxwell asked for the Mac build now, so M8 starts alongside Phase 1 instead of after Windows v1 ships. This changes D8's order, not its scope: Windows stays the main test machine, and shared code stays OS-neutral.
- **What it means:**
  - `grape-platform`'s `macos` module gets real code: drag-pasteboard polling (D7, no permissions), display bounds, file actions and haptics, following `PLATFORM_MACOS.md`.
  - The app wires the overlay for macOS.
  - CI lints and tests on `macos-latest` and builds an ad-hoc-signed, un-notarized universal DMG as a downloadable artifact on every run.
  - Notarization and the release workflow stay in M9.
- **Details decided while building it:**
  - **Coordinates.** On macOS, `grape-platform`'s global coordinates are points with the origin at the top-left of the primary screen, y down (the `CGDisplayBounds` space), and `DisplayBounds.scale` is 1.0. Windows keeps physical pixels.
  - **Drops.** wry's own drop handler is the overlay's drop target on macOS (Tauri's `WindowEvent::DragDrop`). Unlike WebView2 it accepts the drag as a copy, so D13's replacement isn't needed there. The app offsets its window-relative points by the overlay's frame.
  - **Showing windows.** The overlay and toast are shown with `orderFrontRegardless` and can't become key (`focusable: false`). The overlay sits at level 101 (`.popUpMenu`) and the toast at 100, so an open wheel always covers the toast. Never a level of 500 or more.
  - **Move never moves a mounted volume's root** (a USB stick, DMG or share dragged from the Desktop). Moving one would copy its contents and then erase them, without the other-drive question.
  - **Two controller rules for macOS only** (`ControllerConfig` flags, off on Windows): run the highlighted segment on release when Command or Control was held and no drop arrived (AppKit may refuse those drops), and treat a `Leave` with the button still down on the same display as Esc after 200 ms (Grape can't read Esc without permission). A button-up counts as a refused drop only within 50 ms of a `Leave`, so Esc and then letting go runs nothing.
  - **No pasteboard reads.** The monitor checks only the drag pasteboard's change count and types. The paths come from the drop.
  - **New dependencies, macOS only:** `objc2` 0.6 (MIT), `objc2-foundation` 0.3 (MIT), `objc2-app-kit` 0.3, `objc2-core-graphics` 0.3 and `dispatch2` 0.3 (each Zlib OR Apache-2.0 OR MIT). All were already in `Cargo.lock` through tao and wry; `grape-platform` now names them.
- **Tested:** CI only, until Maxwell runs the DMG on a Mac. Every Mac behavior that CI can't check has a `mac:` line in `TESTING.md`.
- **Revisit if:** the Mac work starts slowing Windows v1 down.

## D19: Settings, first slice (2026-10-24, proposed, confirm)
- **Decision:** the Settings window (M6) shows real settings. Every change applies at once (no Save button) and is saved in `settings.json` in the app's data folder through `tauri-plugin-store`, with the keys in `ARCHITECTURE.md` → Settings.
  - **Keys.** Each wheel's key is a choice of Shift, Ctrl and Alt on Windows (never the Windows key, which opens Start), and Shift, Control, Option and Command on macOS. The three wheels always have three different keys: choosing one another wheel uses swaps the two, and Settings says so ("Ctrl now opens Conversions, and Shift opens Tools."). The controller takes the new keys for the next modifier; an open wheel isn't touched.
  - **Where files go.** Next to the original, or a folder the user picks in the native folder picker (`grape_platform::file_actions::pick_folder`, the same picker Move uses). The fallback folder can be picked too. A picked fallback folder is used only while it exists, so the engines still only ever create Grape's own `Downloads/Grape`.
  - **Sounds and haptics** are separate settings now; haptics is macOS only.
  - **Launch at login** uses `tauri-plugin-autostart`. ARCHITECTURE's default is on, so a release build turns it on at its first launch when nothing is stored. Debug builds never do this on their own, and macOS skips it when Grape runs from the DMG or a translocated copy. Settings shows the real state.
  - **Bad settings never cost the user's file.** A missing or unusable value falls back to its default, with a log line that names the key but never the value. A `settings.json` that can't be read is moved aside as `settings.unreadable.json` before anything can write over it.
  - The UI never sends a path: folders are always picked on the Rust side. The commands are granted to the Settings window only.
  - **Terminal app** (second slice): Windows Terminal or PowerShell on Windows, Terminal or iTerm2 on macOS. The fallbacks stay: PowerShell when `wt` can't start, Terminal when iTerm2 isn't installed.
  - **Welcome card** (second slice): until "Got it" stores `onboarding.done`, Settings opens at launch with a three-step "drag, tap, drop" card that names the user's keys. It doesn't open when settings can't be saved (it would open at every launch), or when Grape started at login (launch at login passes `--at-login`), so it never jumps in front of what the user opens after signing in.
  - Not yet: `features.v2` (a stored value is left alone).
- **New dependencies:**
  - Direct, pinned exactly: `tauri-plugin-store` 2.5.0 and `tauri-plugin-autostart` 2.7.0 (both Apache-2.0 OR MIT).
  - New in `Cargo.lock`: `auto-launch` 0.6.0, `smappservice-rs` 0.1.3, `os_info` 3.15.0, `nix` 0.31.3, `cfg_aliases` 0.2.2 and `tokio-macros` 2.7.2 (MIT); `dirs` 6.0.0 and `windows-registry` 0.6.1 (MIT OR Apache-2.0); `objc2-service-management` 0.3.2 and `objc2-security` 0.3.2 (Zlib OR Apache-2.0 OR MIT).
  - All were published more than 14 days ago (D9). `dirs` 6 reuses the `option-ext` that O6 already covers.
  - `grape-platform` now uses `log` 0.4 too (already in D9), to say why an action did something other than what was asked.
- **Revisit if:** remapping should allow two keys at once, or Maxwell wants launch at login off by default.

---

## Open questions

- **O1: Confirm the open-source license.** MIT is assumed. GPL-3.0 is the alternative if you want forks to stay open. Note: the repo's `LICENSE` file is already MIT ("Copyright (c) 2026 sussyswimmer"), and `Cargo.toml` and the bundle say MIT.
- **O2: Apple Pages support.** There's no open library and `NSAttributedString` can't read `.pages`. How Tangerine does it is unknown; the website doesn't reveal it. Options: (a) skip; (b) macOS-only via scripting the Pages app if installed; (c) parse the IWA format (large effort). Default: skip until users ask.
- ~~**O3: Windows test hardware.**~~ Resolved: Maxwell has his own Windows PC (see D8).
- **O4: Optional add-on downloads** (LibreOffice for slides/office in v2). This needs a network call outside the conversion path. Decide before v2.
- ~~**O5: Show the wheel for drags from any app on Windows,** or only Explorer and the Desktop (the default)?~~ Answered by default in D14: Explorer and the Desktop only, with "Any app" as a possible setting in M6. Confirm or change.
- **O6: MPL-2.0 crates linked by Tauri.** Tauri 2.12 statically links `option-ext` 0.2.0 (MPL-2.0) through `dirs` → `dirs-sys`. ENGINES rule 1 allows only MIT/Apache/BSD/ISC/Zlib, or LGPL linked dynamically. MPL-2.0 is file-level copyleft: linking it into an MIT app is fine as long as the MPL files' source stays available, which it does (unmodified on crates.io). Proposal: amend rule 1 to allow unmodified MPL-2.0 crates, list them in `THIRD_PARTY_NOTICES.md` in M7, and add a `cargo-deny` license check to CI.
- **O7: Wheel titles.** `TOOLS_AND_ACTIONS.md` lists the wheel titles (METADATA, ADD BG, BLEEP, …) as **[observed]** from the demo, while golden rule 1 bans Tangerine's wording. `grape-core` encodes them exactly as the doc lists them, in one `match` checked by the doc-sync test, so rewording them later is a one-line change on each side. Keep them, or write our own?
