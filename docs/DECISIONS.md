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

- **`windows` features** grow with the milestones, never as new crates. D13 lists M1's. The M5 file actions add `Win32_System_DataExchange` and `Win32_System_Memory`, which tao already enables on the same version, and `Win32_Storage_FileSystem` (bindings only, for `GetVolumePathNameW`). Share and the float shelf add `Foundation`, `ApplicationModel_DataTransfer`, `Storage`, `Storage_Streams` and `Win32_UI_Shell_Common` (D21).
  - **The one exception is `windows-collections`** (D21). The share sheet takes an `IIterable<IStorageItem>`, and only that crate can make one from a `Vec`. The `windows` crate builds it but doesn't re-export it, and Windows has no collection class to use instead. It was already in `Cargo.lock`, so no new package came in.

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
- **Other windows:** Settings gets the same target in refusing mode (`refuse_drops`), answering `DROPEFFECT_NONE` to everything, because tao's and wry's handlers there have the same unset effect. The toast refuses drops too, and so does the float shelf, at creation and before every show, so a drag out of it dropped back on it does nothing (D21). Every future Grape window needs one or the other.
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

## D20: Subtitle conversion rules (2026-10-26, proposed, confirm)
- **Decision:** SRT ⇄ WebVTT and SRT/WebVTT → TXT run through Grape's own parser and writers in `grape-engines` (`src/subtitle/`), with no new crate. `ENGINES.md` → "Subtitles" has the full rules. The choices worth a second look:
  - **Text encoding.** A BOM decides first (UTF-8, UTF-16LE, UTF-16BE), then valid UTF-8. Anything else is read as **Windows-1252**, which is right for most old Western-European SRTs. Cyrillic, Greek, Arabic, Chinese and Japanese files in a legacy code page come out garbled. When a file has no readable cue, the log names the encoding it was read as. Detecting those properly would need `encoding_rs` + `chardetng` (MIT/Apache); revisit if users hit it.
  - **What's written.** Output is always UTF-8. An SRT gets a UTF-8 BOM unless the source was BOM-less UTF-8, because Windows players otherwise guess the ANSI code page. WebVTT and TXT get no BOM. Line endings follow the source (CRLF or LF).
  - **Styling.** Italic, bold and underline survive both ways (`<i> <b> <u>`, and SRT's `{\i1}`-style codes). Everything else is dropped but its text kept: SRT `<font>` colors, positioning codes like `{\an8}`, WebVTT classes, voices (`<v Name>`, so the speaker's name is lost), language spans, cue settings and `STYLE`/`REGION` blocks. Ruby annotations are written in parentheses: 漢(かん).
  - **TXT** is the spoken text only: one line per subtitle line, no timings, no blank lines, and a line that repeats the one before it is written once (auto-generated captions repeat each line as it scrolls).
  - **Messy files are read leniently**: missing blank lines, `.` or `,` before the milliseconds, missing hours, out-of-order cues (they're sorted, as WebVTT requires) and cues that end before they start (given 1 ms). A file with no readable cue is refused.
  - **Size:** a subtitle file over 64 MiB is refused unread.
- **Why:** both formats are simple enough that a small, fully tested parser is safer than a dependency, and every choice above can change in one place.

## D21: Share and the Float shelf on Windows (2026-10-25, proposed, confirm)
- **Decision:** on Windows, SHARE opens the system share sheet with the dropped files, and FLOAT puts the dropped items on a small shelf window. Items drag out of the shelf as copies or shortcuts, never as a move. macOS keeps AirDrop for Share. Float on the Mac comes later.
- **Share:**
  - Files only. Folders are left out, and so are missing items and shortcuts (`.lnk`, `.url`, `.wsh`), which `StorageFile` can't represent. The rest are looked up with `StorageFile::GetFileFromPathAsync`, all at once, with 10 s for the lot, in the multithreaded apartment, which Grape keeps for the app's life (`CoIncrementMTAUsage`) so the files the sheet holds stay usable. A file that fails or is late is left out too. A `\\?\` prefix is stripped before the lookup.
  - A toast says what was left out, for example "Sharing 2 of 3 / Folders can't be shared." When nothing is left, no sheet opens, and the toast says "Nothing to share".
  - **The anchor window.** The sheet belongs to a window of Grape's own, on a thread of its own: "grape-share", a single-threaded COM apartment with a message loop, started on first use and kept for the app's life.
    - The anchor is a 32 × 32 logical px popup centered where the wheel was. It's `WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT` at alpha 1 of 255, so it has no taskbar button or Alt-Tab entry, can't be seen and lets clicks through.
    - It's neither topmost nor `WS_EX_NOACTIVATE`, and it's shown with `SW_SHOWNOACTIVATE`. Grape never takes the focus to open the sheet.
    - If the sheet activates the anchor when it closes (after it asked for the files, filled or not), the anchor hands the foreground back to the window that had it, then hides. It's never destroyed, because a sheet may still be open on it.
  - The `DataRequested` handler follows Chromium's rules: when it's removed, a title that's never empty, `Copy`, the files read-only, `FailWithDisplayText` on a failure, and always `Ok`. If it hasn't run 30 s after the call (Chromium's limit), Share fails with "Couldn't share" and the handler is removed. Every HRESULT on the way is logged.
  - One share runs at a time. A second one waits for the first to be served or to give up.
  - **Why:** Chromium, Firefox and Microsoft's WPF ShareSource sample all share from an unpackaged desktop app through `IDataTransferManagerInterop::ShowShareUIForWindow`. Each one passes a live, visible, activatable window of its own process. The docs set no rules for that window. Grape's toast is `WS_EX_NOACTIVATE`, the case no source covers, and tao holds back every event while a main-thread task runs, so the anchor is a new window on a thread of its own. Files (the open-source file manager) leaves folders out of its shares too, and nobody has checked whether Nearby Share or Mail take them.
- **Float shelf:**
  - It holds at most 50 items, in drop order, and never the same path twice. A toast says so when some didn't fit.
  - It opens at the right edge of the drop's monitor work area, 12 logical px in, centered top to bottom, without taking focus. An open shelf stays where it is and only changes height.
  - Moved by its header and left within 48 logical px of the work area's left or right edge, it snaps flush to that edge, 300 ms after the last move and once no button is down. It's always kept inside the work area.
  - Items stay on the shelf after a drag. Remove and Close take items off the shelf and never touch the files. The shelf lives in memory only, so quitting clears it.
  - It can't be minimized or maximized, and Alt+F4 on the active shelf is the header's ×: the close is prevented, and the shelf is emptied and hidden. So the window is never destroyed with its items kept, and never left minimized out of reach (a tool window has no taskbar button). A shelf minimized anyway counts as closed: the next Float places it again, and `show_shelf` restores it.
  - Double-click or Enter shows an item in Explorer. On an item that's gone it does nothing, since Explorer would open some other folder; Rust checks, as the page's Missing mark can be stale.
  - It refuses drops (`refuse_drops`, D13), so a drag out of it dropped back on it does nothing. Adding items by dropping them on the shelf is a later idea.
  - It's a tool window (no taskbar button, not in Alt-Tab) that a click activates. Only the foreground window can capture the mouse, and the drag out needs the capture. Grape still never activates it itself: `show_shelf` shows it with `SWP_NOACTIVATE`. `harden_shelf` uses D11's subclass with `WS_EX_TOOLWINDOW` on and `WS_EX_APPWINDOW` and `WS_EX_NOACTIVATE` off, behind the same `overlay-subclass` feature.
  - **Drag out:** `SHDoDragDrop` with the shell's own drop source and drag image, offering copy or link, never move (golden rule 2). The data object is Grape's own `IDataObject`: it serves `CF_HDROP` and `Preferred DropEffect` = copy itself, and never serves or stores the shell ID list formats (`Shell IDList Array`, `Shell Object Offsets`; CI checks `CFSTR_SHELLIDLIST`). An inner, empty `SHCreateDataObject` keeps what callers store on the object (the drag image, a target's `Performed DropEffect`, a drop description), and only formats stored that way are served back or listed: even empty, that object makes up formats of its own (a shell ID list, maybe file names).
    - The first build used an empty `SHCreateDataObject` with the two formats set on it. On real Windows that object answers `CFSTR_SHELLIDLIST` anyway, and the CI test caught it, hence Grape's own object.
    - `float_drag_out` is an async command. On its own thread it leaves out the items that are gone, so a share that stopped answering never stalls the main thread with the button held, then it queues the drag on the main thread. A synchronous command would run the drag loop inside WebView2's IPC callback.
    - The button is read before the paths are looked at and once more after, just before the drag starts. If it's up, nothing starts: the drag would drop at once wherever the pointer is.
    - No drag starts from the header's move region: Chromium refuses to start a drag during a move loop.
    - A watchdog sends Esc to the drag once if no button has been down for 3 s: to the window holding the drag thread's mouse capture, or, when none does, to the drag thread itself (`PostThreadMessageW`). That ends a drag that missed the release, which would otherwise hold up the whole app (drag-rs issue #94).
  - **Why this data object:** Electron drags file lists as `CF_HDROP` alone, offering copy and link, and Chromium's file drags are `CF_HDROP` too. `CF_HDROP` has no common-parent rule, so files from several folders work, and Grape already builds the block for Copy. The object is Grape's own so that it alone decides which formats it answers.
  - **The Recycle Bin argument:** OLE masks what a target answers with what the source allows. A target that asks for a move then gets nothing, and its `Drop` isn't called (Wine's `ole2.c`). On Windows that layer is closed source. ReactOS's clean-room Recycle Bin target deletes the files of any drop that carries a shell ID list itself, in `Drop`, without looking at the allowed effects, and with Shift the delete is permanent. Refusing the ID list is the second guard. CI checks the refusal, but neither guard has been checked against Windows' own Recycle Bin, so a drop on it, plain and with Shift, is the first Float check on the PC, on a scratch file.
  - **macOS:** Float stays "Not in this build yet", because a drag out there needs an `NSDraggingSession`. The shelf window is never built on macOS or Linux.
- **New dependencies** (Windows only):

| Crate or feature | Version | License | Why |
|---|---|---|---|
| `windows` features `Foundation`, `ApplicationModel_DataTransfer`, `Storage`, `Storage_Streams` | 0.62.2 | MIT OR Apache-2.0 | The share sheet's WinRT types (`DataTransferManager`, `DataPackage`, `StorageFile`, `IStorageItem`). Nothing else in the build enables them, so they add new compiled bindings |
| `windows` feature `Win32_UI_Shell_Common` | 0.62.2 | MIT OR Apache-2.0 | `SHCreateDataObject`, the inner store of the drag-out data object for the formats callers set on it (the drag image). Also new compiled bindings |
| `windows-collections` | 0.3.2 | MIT OR Apache-2.0 | `IIterable<IStorageItem>` from a `Vec`, which `SetStorageItems` takes. The `windows` crate builds it but doesn't re-export it, and Windows has no collection class to use instead. It was already in `Cargo.lock` as a dependency of `windows`. The one exception to D9's "never as new crates" |

- **Tested:** `cargo test` on Linux, and clippy for the Windows target and both Mac targets. The Windows-only tests are type-checked and first run on CI's `windows-latest` runner. They check that a temp file becomes a storage item, a folder and a missing path are skipped with no lookup, and a `.lnk` can't be shared. They also check the anchor's styles, the drag data object for files in two folders (both paths as `CF_HDROP`, `Preferred DropEffect` = copy, no shell ID list, both fixed, a fresh block per `GetData`, a shell ID list or file names never served or listed, a stored private format served back, no advise sinks), that the allowed effects have no move bit, a button let go during the path check, the button-up and wrong-thread guards, the shelf's styles on a real window, and that `show_shelf` restores a minimized shelf. On every OS, the app's tests check that a drag out leaves out the items that are gone. No test opens the sheet or starts a drag. Nothing has run on the PC yet.
- **Needs the PC** (`TESTING.md` → "File actions, M5"):
  - first, the Recycle Bin drops;
  - whether the sheet accepts the anchor, where it appears, where the focus goes after it closes, and Nearby Share;
  - whether a click on the shelf gets the mouse capture for the drag (the press lands in WebView2's child window, which belongs to another process);
  - Shift and Alt drops out of the shelf, Esc during one, and one let go over another app (the watchdog);
  - Alt+F4, Win+Down and Win+Up on the active shelf;
  - the snap, and two monitors at different DPI.
- **Revisit if:** the sheet won't open from the anchor (try an activatable popup on the main thread, then the toast), share targets turn out to take folders, a drag out of the shelf ever moves or deletes a file, or the Mac gets a drag out.

## D22: The FFmpeg engine and its pinned sidecar (2026-10-26, proposed, confirm)
- **Decision:** audio and video (104 v1 pairs) convert through **FFmpeg 9.0.2**, shipped as separate `ffmpeg` and `ffprobe` executables (golden rule 6). `ENGINES.md` → "FFmpeg build", "How Grape runs FFmpeg" and the target table have the details.
- **The pin.** *Amended by D24 (2026-10-26):* what ships is Grape's own build of 9.0.2, made from source by `scripts/ffmpeg/build.sh`. The prebuilt builds below stay in `scripts/sidecars.lock` for local testing only, until CI builds D24's and M7 points the lock at Grape's own release. They never ship.
  - 9.0.2 was released 2026-09-18, so it meets D9's 14 days. It's the only release that both prebuilt sources below offer, and BtbN builds its branch.
  - **Windows x64:** BtbN `https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-09-30-13-08/ffmpeg-n9.0.2-17-g2a571b6068-win64-gpl-9.0.zip`, SHA-256 `a0e45723c72141975f51d8666302e614711745f3102b704ca3f82c897a58d278` (194 MB). `bin/ffmpeg.exe` and `bin/ffprobe.exe` are static, about 165 MB each, plus `LICENSE.txt` (GPLv3). It's the tag plus 17 fixes on `release/9.0`. BtbN keeps month-end builds for about two years; never pin a daily build or `latest`, which move.
  - **macOS:** martin-riedl.de 9.0.2, static, macOS 12 or later. Each zip holds one file:
    - arm64 `https://ffmpeg.martin-riedl.de/download/macos/arm64/1789931890_9.0.2/ffmpeg.zip` `c8ed4c4e6978a03c485edbfe4e0a5dc2380f8a30bba5150531b31b094492d924`, `…/ffprobe.zip` `fcbe839537485eaee7a7a8bc5cbc0f90d53617e80943e8a5b2e31cb851197ea6`;
    - amd64 `https://ffmpeg.martin-riedl.de/download/macos/amd64/1789931006_9.0.2/ffmpeg.zip` `7c6b4125b191cbf773832dc51f424cf2b6bb7da43007d1e066f95909e47cacd4`, `…/ffprobe.zip` `2322438ed2f6319a691291b247d09c69dcaa3a982460d1f269a7e1af335cfdfd`.
    - `lipo` joins each pair into a universal file, which voids the upstream signature, so it's re-signed. Riedl's Intel release builds stop in January 2027.
  - **Not chosen:** gyan.dev (228 MB executables, no published build scripts, only the last two releases kept), evermeet.cx (Intel only, no checksums), osxexperts (unversioned URLs).
  - **Fetching, bundling and finding the files** are lane A's (D25): a lock file, scripts that check each hash before extracting, `externalBin` in a build overlay, and `SidecarPaths` next to the executable.
- **GPL obligations (M7, before any build ships the binaries):** *Amended by D24:* they're now those of Grape's own GPL-2.0-or-later build: `licenses/ffmpeg/` and the source bundle the build writes, `grape-ffmpeg-sources.tar`. What follows was the plan for the prebuilt builds. FFmpeg's license text and the bundled libraries' notices, and the corresponding source. GPLv3 §6(d) keeps the source our responsibility even when someone else hosts it, so the release carries it: for Windows the source bundle BtbN's `download.sh` builds at BtbN commit `6c9aec5f…`, for macOS Riedl's build script and the sources it fetched. The lasting fix is our own trimmed CI build (much smaller executables, about 10 library notices instead of about 80).
- **How it runs** (to confirm): FFmpeg may open only plain local files of the ten containers Grape reads (playlists renamed `.mp4` are refused); one video and one audio stream are kept; MP4/MOV/MKV copy compatible streams instead of re-encoding, so an H.264 MKV becomes an MP4 in seconds (this goes beyond the earlier table, which re-encoded MP4 and MOV always); two FFmpeg jobs at a time; no console window; cancel kills the process.
- **Tuning against the earlier table:** WebM gets `-deadline good -cpu-used 4` (the default is very slow); AVI gets constant-bitrate MP3 and the XviD tag; MP3 gets ID3v2.3; WAV/AIFF/FLAC keep 24 bits only from a lossless source; WMA and WMV audio are held to stereo and 48 kHz (wmav2 refuses more); GIF is two passes through a palette file instead of one filter graph, which held the whole clip in memory.
- **The app** keeps calling `supports`, which stays "runnable with no sidecar", so audio and video keep saying "coming" until it passes the sidecars and switches to `supports_with` (lane A).
- **Known limits:** first audio track only; HDR becomes washed-out SDR; no warning for GIFs over 30 s; no hardware encoders yet; a Quit while FFmpeg runs needs the app to cancel the job first (a Windows Job Object would need `unsafe`).

## D23: Third-party notices and the license check (2026-10-25, proposed, confirm)
- **Decision:** `THIRD_PARTY_NOTICES.md` at the repo root lists the code by other people that Grape ships, with each license's full text. cargo-about generates it from `Cargo.lock`. CI checks that it's up to date, and checks every crate's license and source with cargo-deny. Both bundles ship it next to Grape's own license.
- **What the notices cover:**
  - The `grape` app crate's normal dependencies for the three targets Grape ships (`x86_64-pc-windows-msvc`, `aarch64-apple-darwin`, `x86_64-apple-darwin`): 262 crates today. Build and dev dependencies are left out, and so are Grape's own crates, which `LICENSE` covers.
  - cargo-about counts a proc-macro's dependencies as normal ones, so a few crates that only run while Grape is built are listed too. Four are MPL-2.0: `cssparser`, `cssparser-macros`, `dtoa-short` and `selectors`, through `tauri-macros`. Listing too much does no harm, and the file says that `option-ext` is the one MPL crate compiled in.
  - When a crate offers a choice, Grape takes MIT, then Apache-2.0, BSD-3-Clause, Zlib and Unicode-3.0, in that order (`about.toml`). Those five, plus MPL-2.0 for five crates, are every license in the file.
  - The JavaScript in the built UI: `@tauri-apps/api` 2.12.1 (taken as MIT) and Vite 8.3.3's module preload polyfill (MIT). A fresh `vite build` has no other license banners or bundler helpers. That section is written by hand in `about.hbs`, so check it again when the UI's dependencies or Vite change.
  - **Not yet:** the sidecars and their DLLs (D22). Each gets a folder in `licenses/` and an entry in `about.hbs` (`licenses/README.md`).
- **Clarifications** (`about.toml`): 25 crates point cargo-about at their real license text, each file pinned by SHA-256 so a changed file fails the run.
  - windows-rs (`license-mit`) and the Tauri plugins (`LICENSE_MIT`) name their files in ways cargo-about doesn't scan.
  - `dpi` has libm's MIT text with its copyright lines. `brotli-decompressor` ships only the BSD text of its two options, so Grape takes BSD-3-Clause. cargo-about took a copied code file in `schemars_derive` for a license.
  - `webview2-com`, `webview2-com-sys`, `webview2-com-macros` and `alloc-stdlib` ship no license file, so the text comes from their GitHub repository at the commit each was published from. That's the one network call generating the file makes (raw.githubusercontent.com), so regenerating needs the network.
  - The objc2 crates (macOS only) have no license text anywhere, and `dunce` ships only its CC0 option. The file shows the standard MIT and Apache-2.0 texts for them, and says so.
- **The license check** (`deny.toml`: the same three targets, dev dependencies left out, build dependencies in):
  - `licenses` allows exactly MIT, Apache-2.0, BSD-3-Clause, Zlib and Unicode-3.0, and an allowed license that nothing uses is an error. MPL-2.0 is allowed only for `option-ext` and the four build-time crates above, by name. No GPL, AGPL or LGPL (golden rule 6).
  - `bans` warns about duplicate versions and never fails on them (nine crates have two or three today). Wildcard versions fail, except Grape's own path dependencies.
  - `sources` allows crates.io only. Any other registry or a git source fails.
  - `advisories` runs as a separate CI step that can't fail the job, because it fetches the RustSec database on every run.
- **CI:** a `Licenses` job on `ubuntu-latest` installs both tools with taiki-e/install-action (prebuilt binaries checked against its SHA-256 list, no fallback). It runs the check, regenerates the notices, and fails with the regenerate command if `THIRD_PARTY_NOTICES.md` changed.
  - `.gitattributes` marks the file `-text`, so git keeps it byte for byte. generic-array's license text has CRLF line endings. With the repo's `eol=lf`, a fresh checkout would have LF there, and the regenerated file would never match it.
- **Shipping:** `bundle.resources` in `tauri.conf.json` maps `THIRD_PARTY_NOTICES.md` and `LICENSE` (as `LICENSE.txt`, so Windows opens it) to the resource root. That's the install folder on Windows (`%LOCALAPPDATA%\Grape` for the per-user installer) and `Grape.app/Contents/Resources` on macOS.
  - The map form is what keeps them there. A plain list puts a `../` path under `_up_` folders (tauri-utils 2.10.1, `resources.rs`). tauri-bundler 2.10.1, which CLI 2.12.1 uses, installs a map's targets as given in the NSIS installer, and copies them into the `.app`'s `Resources`. The uninstaller removes them.
  - tauri-build checks the paths on every build and copies the files into `target/<profile>`, so `cargo check -p grape` fails if one is missing.
- **Tools** (developers and CI only; they never ship and nothing links them):

| Tool | Version | Published | License | Why |
|---|---|---|---|---|
| cargo-about | 0.9.2 | 2026-08-18 | MIT OR Apache-2.0 | Generates `THIRD_PARTY_NOTICES.md` |
| cargo-deny | 0.20.2 | 2026-07-09 | MIT OR Apache-2.0 | The license, ban and source check |
| taiki-e/install-action | v2.87.27 (commit `ab68953`) | 2026-10-09 | Apache-2.0 OR MIT | Installs both in CI from checksummed prebuilt binaries |

  - Each is the newest release at least 14 days old (D9). v2.87.28 was 13 days old.
  - To install them locally: `cargo install --locked --features cli cargo-about@0.9.2` (the binary needs the `cli` feature) and `cargo install --locked cargo-deny@0.20.2`.
- **Revisit if:** Maxwell decides O6 another way (then drop the MPL exceptions and the dependency with them), a clarified crate's file changes (its checksum fails), a sidecar ships (D22), or the UI gets a runtime dependency.

## D24: Grape builds its own FFmpeg (2026-10-26, proposed, confirm)
- **Decision:** the `ffmpeg` and `ffprobe` that Grape ships are Grape's own build of FFmpeg 9.0.2, made from source by `scripts/ffmpeg/build.sh <target>` from the tarballs pinned in `scripts/ffmpeg/sources.json`, with only what Grape uses. They replace the prebuilt BtbN and martin-riedl.de builds of D22 as what ships. The engine (D22) doesn't change. `scripts/ffmpeg/README.md` says how to run the script, and `ENGINES.md` → "FFmpeg build" what's in the build.
- **Why:**
  - The prebuilt builds can't ship as they are. Both are GPL-3.0 (`--enable-version3`) and link libzvbi, whose `packet-830.c` and `pdc.c` are GPL-2.0-only: code that can't be combined with GPL-3.0 code. They're in the macOS binaries, and almost certainly in the Windows one (the `lane-b/ffmpeg-licenses` research).
  - They link about 100 libraries (Windows) and 33 (macOS), for things Grape never does: network protocols and TLS, SSH, SRT, ZeroMQ, subtitle rendering and fonts, DVD and Blu-ray, hardware APIs, hundreds of filters. Each needs its notice, and some (rav1e, librsvg) carry Rust crates whose notices nobody had collected. `licenses/ffmpeg/` would have needed about 160 texts; it has 18.
  - Their network code is in the binary. Grape's `-protocol_whitelist file` keeps it unused, but golden rule 3 is easier to keep when the code isn't there at all.
  - The corresponding source: for BtbN and Riedl we'd have had to rebuild their source bundles ourselves (D22). Ours comes out of the build.
  - Size: BtbN's `ffmpeg.exe` is about 165 MB; ours is 28 MB.
- **What's built:** FFmpeg with `--disable-everything` and `--disable-autodetect` (nothing gets in because the build machine happens to have it), then only:
  - the protocols `file` and `pipe`, and `--disable-network`: no TCP, HTTP or TLS code. `ffmpeg -protocols` lists those two;
  - the ten demuxers Grape reads (its `-format_whitelist`) and `image_png_pipe` for the GIF palette; the fourteen muxers it writes (WMV and WMA share `asf`) and `image2pipe`;
  - every decoder, parser and bitstream filter (509, 68 and 51), because people drop every kind of file and stream copies need the bitstream filters. Only the external libraries' Opus, Vorbis and VPx decoders are left out: FFmpeg's own are picked first anyway;
  - exactly the sixteen encoders Grape uses, and H.263, which FFmpeg's MPEG-4 encoder is built on;
  - the filters Grape passes, those ffmpeg inserts by itself (rotation, crop, trim, format and alpha conversion), and `sine`, `testsrc` and `pan` with the `lavfi` input device, which only the engine tests use to make their sources. Grape's argv can't reach `lavfi`: the only input format it names is `png_pipe` (the GIF palette), and the source's `-format_whitelist` lists only file demuxers;
  - `build.sh` checks the component lists configure generates against these, exactly, and stops on any difference.
- **The libraries,** each the newest release at least 14 days old (D9), each built as a static library from its release tarball:

| Library | Version | License | Why |
|---|---|---|---|
| x264 | stable branch, `b35605ac` (2025-06-08) | GPL-2.0-or-later | H.264 for MP4, MOV and MKV. 8-bit only |
| LAME | 4.0 (2026-07-11) | LGPL-2.0-or-later | MP3, and the MP3 audio in AVI. No decoder, no frontend |
| Opus | 1.6.1 (2026-01-13) | BSD-3-Clause | Opus, and WebM's audio |
| libvorbis, libogg | 1.3.7 (2020-07-04), 1.3.6 (2025-06-16) | BSD-3-Clause | OGG (Vorbis) |
| libvpx | 1.17.0 (2026-08-07) | BSD-3-Clause | WebM's VP9 video. The VP9 encoder only |
| dav1d | 1.5.4 (2026-07-14) | BSD-2-Clause | AV1 sources: FFmpeg's own AV1 decoder only drives hardware |
| zlib | 1.3.2 (2026-02-17) | Zlib | PNG (GIF palettes, PNG in MOV), compressed MOV headers and Matroska tracks |

  - FFmpeg's own encoders do the rest: AAC, FLAC, PCM, MPEG-4 (AVI), WMV2 and WMA2, GIF and PNG.
  - **GPL-2.0-or-later.** No library is GPL-3.0 or Apache-2.0, so the build takes `--enable-gpl` (for x264) and never `--enable-version3` or `--enable-nonfree`. `licenses/ffmpeg/` has the texts: the GPLv2, each library's, the IJG credit FFmpeg's DCT code asks for, the ISC notice of `x86inc.asm`, and for Windows the mingw-w64 runtime's notices and the GCC Runtime Library Exception (libgcc is linked statically). `SOURCE.md` has the configure line and the pins; the FFmpeg section of `about.hbs` the notices entry.
- **Targets:**
  - `windows-x86_64`: cross-compiled on Linux with mingw-w64's GCC in the win32 thread model, fully static. The exe imports only Windows' own DLLs. libvpx is kept on Win32 threads too (a `pthread.h` that doesn't compile makes its configure take its MSVC path), so no winpthreads.
  - `macos-arm64` and `macos-x86_64`: native builds with Xcode's clang on an Apple silicon and an Intel runner, for macOS 11.0 or later, joined with `lipo` and signed as today (D25). libvorbis's `configure` loses `-force_cpusubtype_ALL`, an option of Apple's old linker: the one change the script makes to a source.
  - `linux-x86_64`: only for the `engines-it` tests.
- **Where it runs:** CI is lane A's: a job per target that runs `scripts/ffmpeg/build.sh <target>` (`windows-x86_64` on ubuntu-24.04, the two macOS targets on their runners), cached on the hash of `scripts/ffmpeg/**` and `licenses/ffmpeg/**` (the source bundle carries both). At M7 the binaries and `grape-ffmpeg-sources.tar` go on Grape's GitHub Release, and `scripts/sidecars.lock` points at them. Until then BtbN and Riedl stay in the lock for local testing only, and D25's gate keeps them out of every bundle.
- **The source bundle:** `dist/grape-ffmpeg-sources.tar` holds every tarball exactly as downloaded, `scripts/ffmpeg/` and `licenses/ffmpeg/`. It's byte for byte the same from every target's run of one commit, and building from it downloads nothing (`GRAPE_FFMPEG_TARBALLS`).
- **Downloads:** each tarball goes into a new, empty folder over HTTPS only; its size and SHA-256 must match `sources.json`, and its file list must pass (one top folder, no absolute path, no `..`, no link that leaves it) before `tar` sees it. Two official URLs couldn't be fetched from the machine that pinned them (downloads.xiph.org for Opus, codeload.github.com for libvpx), so their hashes come from copies that other packagers' records of the official files confirm; `sources.json` says which. A review the same day confirmed both byte for byte from upstream: Opus from a mirror of downloads.xiph.org whose `SHA256SUMS.txt` lists the same hash, and libvpx by recreating GitHub's archive from the `v1.17.0` tag (`git archive` through `gzip -cn`).
- **Tested** on Linux, 2026-10-26 (a shared 4-core container):
  - `linux-x86_64` built in 524 s (FFmpeg itself 305 s): `ffmpeg` 27.98 MB, `ffprobe` 27.76 MB, linked only to glibc. All nine `engines-it` media tests pass against it: the 104 pairs, the hostile playlists, cancel, the names and `the_sidecar_has_everything_grape_uses_and_is_redistributable`. `-protocols` lists `file` and `pipe` only, and `-buildconf` has `--disable-network`.
  - `windows-x86_64` built in 615 s (FFmpeg itself 386 s): `ffmpeg.exe` 28.01 MB, `ffprobe.exe` 27.81 MB, PE32+ x86-64. They import only `KERNEL32.dll`, `SHELL32.dll`, `bcrypt.dll` and `msvcrt.dll`. The link map shows no winpthreads, libssp or libatomic; from GCC only seven small `libgcc` helpers, and from mingw-w64 its startup code and parts of `libmingwex`. Under wine 9.0 both run: `-version` prints the configure line from the build, `-protocols` lists `file` and `pipe`, Grape's own argv turns an MP4 into WebM (VP9 and Opus), MP3 and a two-pass GIF, `ffprobe` reads the WebM, and an HLS playlist renamed `.mp4` is refused.
  - Reproducible: two `windows-x86_64` builds of one commit in different folders came out byte for byte the same (after keeping the folder out of libvpx's build string and the dates out of the PE header). A `linux-x86_64` build from the unpacked source bundle, with nothing downloaded, matched except its GNU build ID. The source bundle (34.9 MB) was the same from the Linux and the Windows run.
  - The script's failures: a wrong size or SHA-256 (the file is deleted, nothing unpacked), a tarball with `..`, a link out or a member outside its folder, bad JSON or an `http://` URL, a work folder it didn't make, a target for another OS.
  - **Flaky under load, not this build:** `every_audio_and_video_pair_converts` failed in 2 of about 30 runs while the machine was loaded (load 11 on 4 cores): WMV to GIF lost its last two frames (0.83 s against 1.09 s). Run alone, the GIF pass loses them about 1 time in 13 with this build and with BtbN's 9.0.2 alike (3 of 40 each, under wine), and never with FFmpeg 6.1. It needs the palette as a second input and a source whose video starts after its audio (the test's WMV starts its video at 0.046 s). So it's FFmpeg 9.0 with D22's two-pass GIF arguments; lane B's to fix.
  - Not tested: the macOS targets (no Mac here; their first run is CI's), and the Windows exe on Windows itself.
- **To confirm:** the source bundle leaves out the toolchain runtimes linked into the Windows exe (mingw-w64's C runtime and a few `libgcc` helpers), as system libraries under GPLv3 section 1, which the GPL-2.0-or-later build may be distributed under. `SOURCE.md` names their exact sources. If you'd rather not lean on that, the bundle can carry mingw-w64 v11.0.1's source too (10 MB).
- **Revisit if:** a pinned library gets a security fix; Grape needs another component (add it to the lists in `build.sh`, which checks them exactly); hardware encoders come (Media Foundation and VideoToolbox are OS frameworks, so no new library; NVENC, AMF and oneVPL each add MIT headers or a dispatcher); the CI image's mingw-w64 or GCC changes (check `mingw-w64-runtime.txt` and `gcc-runtime-exception.txt` against it); or GitHub retires a macOS runner type.

## D25: Fetching and bundling sidecars (2026-10-26, proposed, confirm)
- **Decision:** the sidecar builds are pinned in one file, `scripts/sidecars.lock`, and two scripts fetch them from it: `scripts/fetch-sidecars.ps1` on Windows and `scripts/fetch-sidecars.sh` on macOS. Only the lock is committed. The binaries land in `apps/desktop/src-tauri/binaries/`, which git ignores. The pin itself (FFmpeg 9.0.2: BtbN's build for Windows, martin-riedl.de's for macOS) is D22, lane B.
- **The lock** is JSON (`format` 1, with `_comment` keys for notes):
  - `archives`: each download by id, with its https URL, its SHA-256 and its size in bytes.
  - `sidecars`: each program's version and, per target triple, the archive it comes from and its path inside it, plus a `license` path where the archive has one (BtbN's `LICENSE.txt`). One archive can supply several files: BtbN's zip has both programs.
  - heif-enc goes in the same way once lane B pins it, with a Windows target only. The scripts' version check is FFmpeg's (`-hide_banner -version`, first line `<name> version …`), which heif-enc doesn't answer, so `fetch-sidecars.ps1` needs a check for it first.
- **The checks**, in this order:
  - Downloads use HTTPS only (curl `--proto =https --proto-redir =https`; Invoke-WebRequest with TLS 1.2 or later), into a new, empty temp folder that's always deleted at the end.
  - The archive's size and SHA-256 must match the lock before anything is extracted. A mismatch deletes it and changes nothing.
  - Only the paths the lock names are extracted. Both scripts check every field of the lock by the same rules first: a path may not have wildcards, backslashes, `.` or `..` parts, or start with `/`. The macOS script also rejects a key that appears twice.
  - macOS: the two per-arch files must be arm64 and x86_64. `lipo -create` merges them into `<name>-universal-apple-darwin`, which is signed ad hoc (lipo breaks the build's own signature) and must pass `codesign --verify --strict`. The per-arch files stay too, because tauri-build looks for them while a universal build compiles each arch (tauri-cli 2.12.1 `desktop.rs`, tauri-build 2.7.1).
  - Each program runs with `-hide_banner -version`, on its own OS only. The first line must start with `<name> version` and name the lock's version as a whole token (BtbN prints `n9.0.2-17-g2a571b6068-20260930`, Riedl `9.0.2-https://www.martin-riedl.de`). The configuration line must not have `--enable-nonfree`.
  - Only then do the files replace the old ones in `binaries/`, and the stamp, `binaries/sidecars.stamp`, is written last: the lock's SHA-256, the target, and each file's SHA-256. A run that finds all of them matching says the sidecars are already current and changes nothing. `-Force` / `--force` fetches again.
- The scripts never change `PATH` and install nothing. This is the only download, at build time. Grape itself never downloads anything (golden rule 3).
- `binaries/ffmpeg-LICENSE.txt` is the BtbN archive's GPLv3 text, kept for the bundle step. The scripts never write into `licenses/`, which is committed and written by hand (`licenses/README.md`). The Riedl zips carry no license file.
- **Bundling** uses a config overlay, `apps/desktop/src-tauri/tauri.sidecars.conf.json`, which sets `bundle.externalBin` to `["binaries/ffmpeg", "binaries/ffprobe"]` and adds `licenses/ffmpeg/` to `bundle.resources`. So any build (or `tauri dev`) that passes it fails until that folder exists: the GPL gate below is a check, not just a rule.
  - A build passes it as `--config src-tauri/tauri.sidecars.conf.json` (from `apps/desktop`). The CLI merges it into `tauri.conf.json` as a JSON merge patch (RFC 7396: objects merge, arrays and values replace) and hands it to tauri-build as `TAURI_CONFIG` (tauri-cli 2.12.1 `helpers/config.rs`).
  - Tauri never picks it up by itself (it reads only `tauri.<platform>.conf.json` on its own), so `cargo` builds and today's CI builds don't need the binaries.
  - Installed, the files lose their suffix: `ffmpeg.exe` and `ffprobe.exe` next to `Grape.exe` on Windows, `ffmpeg` and `ffprobe` in `Grape.app/Contents/MacOS` on macOS.
- **The gate:** no build passes the overlay yet, so no installer or DMG carries FFmpeg. It goes on only once all of these exist:
  - lane B's media engine uses the sidecars;
  - `licenses/ffmpeg/` with the license texts and `SOURCE.md` (the exact source link and the configure line). BtbN's build is `n9.0.2-17-g2a571b6068`: 17 commits past the `n9.0.2` tag on FFmpeg's `release/9.0` branch, so its source link is that commit, not the tag;
  - the FFmpeg entry in `about.hbs`, and so in `THIRD_PARTY_NOTICES.md` (golden rule 6, ENGINES rule 3).
  - Then the build jobs in `ci.yml` and `release.yml` get a fetch step and `--config src-tauri/tauri.sidecars.conf.json`.
- **CI:** a `Sidecars` job on `windows-latest` and `macos-latest`.
  - It restores `binaries/` from a cache keyed on the OS and the hashes of the lock and both fetch scripts (a restored folder skips the download and the checks, so a changed script must miss the cache), `actions/cache@v6` (v6.1.0 is from 2026-06-23, D9). Only a run that fetched and checked everything saves one, and the script checks a restored folder against its stamp.
  - It runs the fetch script, then runs it again, which must find the sidecars current: Windows PowerShell 5.1 and then PowerShell 7 on Windows, macOS's `/bin/bash` 3.2 on the Mac.
  - It lists the files with their sizes (and `lipo -info` on the Mac), builds nothing, keeps no git credentials, and has the workflow's read-only token.
- **Testing:** `GRAPE_SIDECARS_NO_RUN=1` skips the `-version` runs (and lipo and codesign on the Mac) and writes no stamp, so the download can be tested on another OS. The next normal run checks everything again. It's for testing only.
- **Tested** on Linux, 2026-10-26:
  - The `.sh`: the lock checks, and the download, hash check and extraction of the four Riedl zips (Mach-O arm64 and x86_64). With stand-ins for `uname`, `lipo`, `codesign` and `arch`, its whole real path: the stamp, a second run, `--force`, a changed or deleted file, Rosetta or not.
  - Its failures, each leaving `binaries/` unchanged and no temp folder: version 9.0.21 or 19.0.2, `--enable-nonfree`, no configuration line, another program's banner, a universal file missing an arch, a wrong SHA-256, a wrong size, a flipped byte in the download, and Ctrl+C.
  - The `.ps1` in PowerShell 7.6.6: the BtbN download, hash check and extraction (PE32+ x86-64, and the GPLv3 text), the stamp, `-Force`, a changed file, a wrong SHA-256, and the `-version` checks against stand-in programs.
  - Neither script has run on a Mac or the PC yet. Windows PowerShell 5.1 and macOS's bash 3.2 first run in CI.
- **Revisit if:** heif-enc is pinned (a Windows-only sidecar needs its own overlay or a `tauri.windows.conf.json`, since tauri-build wants every `externalBin` for the target it builds), Grape ships for Windows ARM64 (`aarch64-pc-windows-msvc`), the Windows sidecars get signed in M7 (the bundler signs them in place in `binaries/`, which changes their hashes, so the next run fetches again), or Riedl's Intel builds end (January 2027).

---

## Open questions

- **O1: Confirm the open-source license.** MIT is assumed. GPL-3.0 is the alternative if you want forks to stay open. Note: the repo's `LICENSE` file is already MIT ("Copyright (c) 2026 sussyswimmer"), and `Cargo.toml` and the bundle say MIT.
- **O2: Apple Pages support.** There's no open library and `NSAttributedString` can't read `.pages`. How Tangerine does it is unknown; the website doesn't reveal it. Options: (a) skip; (b) macOS-only via scripting the Pages app if installed; (c) parse the IWA format (large effort). Default: skip until users ask.
- ~~**O3: Windows test hardware.**~~ Resolved: Maxwell has his own Windows PC (see D8).
- **O4: Optional add-on downloads** (LibreOffice for slides/office in v2). This needs a network call outside the conversion path. Decide before v2.
- ~~**O5: Show the wheel for drags from any app on Windows,** or only Explorer and the Desktop (the default)?~~ Answered by default in D14: Explorer and the Desktop only, with "Any app" as a possible setting in M6. Confirm or change.
- **O6: MPL-2.0 crates linked by Tauri.** Tauri 2.12 statically links `option-ext` 0.2.0 (MPL-2.0) through `dirs` → `dirs-sys`. ENGINES rule 1 allows only MIT/Apache/BSD/ISC/Zlib, or LGPL linked dynamically. MPL-2.0 is file-level copyleft: linking it into an MIT app is fine as long as the MPL files' source stays available, which it does (unmodified on crates.io). Proposal: amend rule 1 to allow unmodified MPL-2.0 crates, list them in `THIRD_PARTY_NOTICES.md` in M7, and add a `cargo-deny` license check to CI.
  - *2026-10-25:* the notices and the check are done (D23). `THIRD_PARTY_NOTICES.md` lists the MPL-2.0 crates with links to their source on crates.io, and says `option-ext` is the only one compiled in. The other four (`cssparser`, `cssparser-macros`, `dtoa-short`, `selectors`) run only inside Tauri's macros at build time. `deny.toml` allows MPL-2.0 for those five by name and nowhere else.
  - Still Maxwell's call: amend ENGINES rule 1 to allow unmodified MPL-2.0 crates. Until then, a new MPL crate fails CI and needs a name added on purpose. Rule 1 also doesn't name Unicode-3.0, which `deny.toml` allows: a permissive license, used by 19 crates (the ICU crates under `url`, and `unicode-ident`).
- **O7: Wheel titles.** `TOOLS_AND_ACTIONS.md` lists the wheel titles (METADATA, ADD BG, BLEEP, …) as **[observed]** from the demo, while golden rule 1 bans Tangerine's wording. `grape-core` encodes them exactly as the doc lists them, in one `match` checked by the doc-sync test, so rewording them later is a one-line change on each side. Keep them, or write our own?
