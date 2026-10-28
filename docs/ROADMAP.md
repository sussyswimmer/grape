# Roadmap

**Windows first** (DECISIONS D8). Phase 1 ships Grape for Windows, built and tested on Maxwell's own Windows PC. Phase 2 ports it to macOS. Because `grape-core` and `grape-engines` are shared, the Mac port is mostly new `grape-platform/macos` code plus packaging.

Each milestone ends with a build you can run and a ticked acceptance list. **The riskiest work comes first:** if the overlay and drag detection don't work on Windows, nothing else matters.

## Agent lanes (Claude Code + Codex)

The work splits into two lanes that touch different crates, so the agents rarely edit the same files.

| Lane | Owns | Why |
|---|---|---|
| **A: Claude Code** (run on the Windows PC) | `grape-platform`, `apps/desktop/src-tauri`, `apps/desktop/ui`, packaging/CI | Needs on-device testing and judgment about OS behavior |
| **B: Codex** | `grape-core`, `grape-engines`, fixtures, `docs/FORMAT_MATRIX.md` tests | Pure, spec-driven, heavily unit-testable; can run on any OS |

Shared contract: the public API of `grape-core` (see `ARCHITECTURE.md`). Changes to it need a note in `DECISIONS.md` and a heads-up in the PR description.

---

# Phase 1: Windows

## M0: Scaffold (lane A)
- [x] Cargo workspace with the three crates plus the Tauri app. `pnpm` workspace for the UI.
- [ ] Tray icon with menu items Settings…, Pause Grape and Quit. No main window, no taskbar button. *(Built; check on the PC.)*
- [ ] Hidden overlay window created at startup with the flags from `ARCHITECTURE.md` + `PLATFORM_WINDOWS.md`. *(Built, and the style code passes its Windows tests in CI; check the running app on the PC.)*
- [x] `grape-platform` has a `windows` module and a stub `macos` module that compiles to "unsupported", so the workspace still builds on a Mac.
- [x] CI: `cargo fmt --check`, `clippy -D warnings`, `cargo test`, UI typecheck on a `windows-latest` runner.
- [x] README with a header image, one-line pitch and Windows build steps.

**Accept:** `pnpm tauri dev` on the Windows PC shows the Grape tray icon, Quit works, and CI is green.

*Status 2026-10-22:* CI is green (Windows lint + tests, Windows UI, macOS check). The app hasn't run on the PC yet: `TESTING.md` → "App shell" lists what to check.

## M1: Drag + overlay spike on Windows (lane A) ⚠️ highest risk
- [ ] Windows `DragMonitor`: `WH_MOUSE_LL` hook on a message-pumping thread + `GetAsyncKeyState` polling + Explorer/Desktop allowlist. *(Built; check on the PC.)*
- [ ] Controller opens the overlay full-monitor on the monitor under the cursor and draws a placeholder ring at the anchor. *(Built; check on the PC.)*
- [ ] `DragDropEvent::Enter/Over/Drop` → log paths, position and the hit-test result. *(Built, from Grape's own drop target; check on the PC.)*
- [ ] Esc closes the overlay and the OS drag continues to a normal drop. *(Built. Explorer cancels its own drag on Esc, so on Windows the drag ends too: D14.)*
- [ ] **Shift-drop onto the overlay never deletes or moves the source.** Check wry's drop effect; add our own `IDropTarget` if needed. Manual test 10× plus a scripted check where possible. *(wry never sets the effect in `Drop`, so Grape has its own target (D13), and a CI test checks it answers copy with Shift, Ctrl and Alt held. The manual 10× test is still to do.)*
- [ ] Two monitors at different DPI scaling place the wheel correctly (if a second monitor is available). *(Built: per-monitor frame and scale; unit-tested with an offset 150 % monitor.)*
- [ ] Measure: modifier → overlay visible. Target under 100 ms. *(Built: the log prints `painted N ms after the key`.)*

*Status 2026-10-22:* everything is built and green in CI (Windows tests, including the drop target and the hook). None of it has run on the PC yet: `TESTING.md` → "Drag + wheel, M1 spike" lists what to try.

**Accept:** drag a file from Explorer, tap Shift, see the ring at the cursor, drop on the top segment → log line with the correct path. Drop off-ring → nothing happens, and the file stays where it was.

## M2: Core model (lane B), runs in parallel with M1
- [x] `Format`, `targets()`, `tools()`, `actions()`, `output_name()`, `wheel_layout()`, `hit_test()`, `plan()`.
- [x] Tests from `FORMAT_MATRIX.md` → "Test assertions" (275 / 178 counts, naming table, tools table).
- [x] Property test: `hit_test` returns `None` for all points in the dead zone, in gaps and outside the ring.

*Status 2026-10-22:* done by Claude Code at Maxwell's request (API additions in `DECISIONS.md` D12). 242 tests, green on Linux and on CI's Windows runner. A doc-sync test fails the build if `FORMAT_MATRIX.md` or `TOOLS_AND_ACTIONS.md` and the code disagree.

**Accept:** `cargo test -p grape-core` green with 100% coverage of the matrix and naming tables.

## M3: Real wheel + v1 conversions on Windows
- [ ] Lane A: overlay renders real segments from `grape-core`, with highlight, center label and tick sound. Empty-state messages use the user's key names.
- [ ] Lane B: engines for images (HEIC via libheif DLLs + `heif-enc` sidecar), audio/video via the FFmpeg sidecar, subtitles, archives, PDF → JPG/PNG/TXT/MD, images → PDF. *Status 2026-10-26:* RAR input (rar → zip/tar/gz, Extract Archive) through Grape's own bsdtar (D27): engine and CI built; tested on Linux, CI runs Windows and macOS; not in the installer until CI builds FFmpeg too (D24); not run on the PC yet: `TESTING.md` → "RAR, M3".
- [ ] Sidecars: a script `scripts/fetch-sidecars.ps1` that downloads the pinned FFmpeg/ffprobe builds, checks their SHA-256, and places them in `apps/desktop/src-tauri/binaries/` with target-triple names. Never commit the binaries. *Status 2026-10-26:* built, not run on the PC or a Mac yet. The pins are in `scripts/sidecars.lock` (D22, lane B). `scripts/fetch-sidecars.ps1` (Windows) and `scripts/fetch-sidecars.sh` (macOS, merged universal) download them, check each archive's SHA-256 before extracting, check `-version` and place them in `apps/desktop/src-tauri/binaries/` with target-triple names (D25). CI's Sidecars job fetches and checks them on both OSes, cached by the hashes of the lock and both scripts. The app finds `ffmpeg`, `ffprobe` and, on Windows, `grape-heif` next to its own executable at startup (`src/sidecars.rs`, never `PATH`) and gives every job a copy; quitting cancels every running drop and waits up to 2 s for them to stop. The lock's FFmpeg builds are for dev testing only and never ship (D24). HEIC on Windows needs nothing from the lock: CI builds Grape's own `grape-heif.exe` from `tools/grape-heif` (D33). `TESTING.md` → "Sidecars, M3". *Status 2026-10-26, later (#27):* the blocker is gone: CI builds Grape's own FFmpeg with lane B's `scripts/ffmpeg/build.sh` (D24), and every installer and DMG carries `ffmpeg`, `ffprobe` and Grape's bsdtar, with `licenses/ffmpeg/` and `licenses/libarchive/`, through one sidecars overlay per OS: `tauri.sidecars.windows.conf.json` (with `grape-heif` too) and `tauri.sidecars.macos.conf.json`. Their gate refuses anything but Grape's own builds (D25). CI's new `engines-it` jobs run lane B's media tests against those builds on Windows and macOS. None of it has run on GitHub yet. Left for M7: the FFmpeg builds as release assets, with the lock pointing at them.
- [ ] Atomic writes, progress events, cancel.
- [ ] Toast: "Making name…" → "Done", with click-to-reveal in Explorer.

*Status 2026-10-23:* the first slice is built. The 20 raster pairs among JPG, PNG, WEBP, TIFF and BMP convert through `grape-engines` (D16), with no-clobber writes and cancel inside the engine. The app (`src/jobs.rs`) plans each drop, shows "Making {name}…" with progress, then "Done", "{ok} of {n} done" or the engine's error, and a click reveals the first output. Every other pair shows "Not in this build yet", and every tool shows "Tools arrive in a later build." None of it has run on the PC yet: `TESTING.md` → "Wheel look and image conversions".

*Status 2026-10-26:* subtitles are built: SRT → VTT/TXT and VTT → SRT/TXT (4 pairs) through Grape's own parser (D20), on the same no-clobber publishing as images. 24 of the 178 v1 pairs now run. Not run on the PC yet: `TESTING.md` → "Subtitles, M3".

*Status 2026-10-26:* archives and PDF run from the wheel too (D35): ZIP, TAR and GZIP among each other (6 pairs), RAR → ZIP, TAR or GZIP with Grape's bsdtar (3), and PDF → JPG, PNG, TXT and MD with PDFium (4). 37 of the 178 v1 pairs now run. The app finds `bsdtar` next to its executable as it finds FFmpeg, and gives every job PDFium and the PDF worker with the other sidecars. No installer or DMG carries bsdtar until CI builds FFmpeg (D24, D27), so RAR runs in `tauri dev` with a built bsdtar and in CI's `rar-it` jobs; every CI installer and DMG carries PDFium. Not run on the PC or a Mac yet: `TESTING.md` → "Archives, M3/M4" and "PDF, M3/M4".

*Status 2026-10-26, later:* the FFmpeg engine is built (D22): all 104 audio and video pairs, checked by the `engines-it` tests against Grape's own FFmpeg 9.0.2 (D24) and against 6.1. The app runs them wherever `ffmpeg` and `ffprobe` sit next to it (lane A's wiring, D35): in `tauri dev` with the sidecars overlay and Grape's own build in `binaries/`. CI's installer and DMG don't carry FFmpeg until CI builds it and D25's gate lets it in, so there they still show "coming". With the archives and PDF above, 141 of the 178 v1 pairs run when every sidecar is there. Left in M3: HEIC, AVIF, SVG and images → PDF (37). Not run on the PC yet: `TESTING.md` → "Audio and video, M3".
*Status 2026-10-26, later still:* HEIC ⇄ JPG, PNG, WEBP, TIFF and BMP (10 pairs) is built (D33), through a helper process on both OSes: Grape's own `grape-heif.exe` (libheif, libde265 and x265, GPL as a separate program) on Windows and ImageIO in `Grape --grape-heif-worker` on macOS. With it, 151 of the 178 v1 pairs run when every helper is there; left in M3: AVIF, SVG and images → PDF (27). CI builds `grape-heif.exe` and its source bundle in the same run as the installer, which carries it, and its `heif-it` jobs run real conversions through it on Windows and ImageIO decodes on macOS; those jobs haven't run yet, and nothing has run on the PC or a Mac (`TESTING.md` → "HEIC, D33"). The `.heif` and `.hif` aliases wait for lane B (grape-core).

*Status 2026-10-26, #27:* CI builds Grape's own FFmpeg (D24), and the installer and DMG now carry it and Grape's bsdtar (D25, D27), so the 104 audio and video pairs and the three RAR pairs run from a CI build too: with every helper there, 151 of the 178 v1 pairs (D35's amendment). A missing `ffmpeg` or `ffprobe` now says "Grape's media converter is missing" / "Reinstalling Grape brings it back.", and a missing bsdtar "Grape's RAR helper is missing", instead of "coming" (D35). None of it has run on GitHub, the PC or a Mac yet: `TESTING.md` → "Sidecars, M3", "RAR, M3" and "Audio and video, M3".

**Accept:** every v1 pair (178) converts a fixture file on Windows (`engines-it` test run on the PC and in CI), and spot-checks by hand pass.

## M4: v1 tools (lane B engines, lane A panels)
- [ ] compress (image/audio/video), removeMetadata, muteVideo, normalizeAudio, audioChannels, extractArchive, splitPDF, mergePDF, createPDF.
- [ ] Panels: compress presets, merge reorder list, channels toggle, normalize target.

*Status 2026-10-26:* the four panels are built (D31): the window, the IPC with golden fixtures, the order list, and remembered presets. No panel opens in a normal build until lane B wires the tool engines; `GRAPE_PREVIEW_PANELS=1` opens them for QA. CI checks the model, the contract and the page; nothing has run on the PC or a Mac yet (`TESTING.md` → "Tool panels, M4").

*Status 2026-10-26:* Extract Archive (ZIP, TAR and GZIP; RAR with Grape's bsdtar), Split PDF, Merge PDFs and Remove Metadata on PDFs run (D35), and Merge PDFs' panel opens in a normal build when PDFium is found. Remove Metadata's toast says what it removed. Compress, Audio Channels and Normalize Volume wait for lane B's FFmpeg tools (D26); `GRAPE_PREVIEW_PANELS=1` still opens their panels. Not run on the PC or a Mac yet: `TESTING.md` → "Archives, M3/M4" and "PDF, M3/M4".

## M5: File actions on Windows (lane A)
- [ ] copy (`CF_HDROP`), move (`IFileOperation` with undo), copyPath, terminal (Windows Terminal → PowerShell fallback). See `PLATFORM_WINDOWS.md`.
- [ ] share (Windows share sheet), float shelf (OLE drag-out works).

*Status 2026-10-22:* copy, copyPath, terminal and move are built, with toasts. CI's Windows runner checks the clipboard contents and a real shell move. The picker, the terminals and undo still need the PC (`TESTING.md` → "File actions, M5").

*Status 2026-10-25:* Share (the share sheet) and the float shelf are built too (D21). Their tests pass on Linux and their Windows code passes clippy; the Windows-only tests run on CI's Windows runner, and they check the Share file lookups and the shelf's drag data but never open the sheet or start a drag. Neither has run on the PC yet; `TESTING.md` → "File actions, M5" starts with the Recycle Bin check. Float is Windows-only for now: on a Mac it still says "Not in this build yet".

## M6: Settings + polish (lane A)
*Status 2026-10-24:* the first slice is built (D19): keys with swap-on-conflict, output location and fallback folder, sounds, haptics on macOS, and launch at login, all saved and applied at once. CI checks it; nothing has run on the PC or a Mac yet (`TESTING.md` → "Settings, M6").
- [x] Key remapping with conflict detection (defaults Shift / Ctrl / Alt). Output location and fallback. Sounds toggle. Launch at login.
- [x] Terminal app choice (`terminal_app`).
- [x] Pause Grape (tray). First-run onboarding: a 3-step animated "drag, tap, drop" card in Grape's own style (D19's second slice).
- [ ] Reduce Motion, light/dark glass following the Windows theme, accessibility labels. *Status 2026-10-25:* Windows contrast themes are handled too: under `forced-colors: active` the wheel, toast and shelf switch to the system's colors, keeping the highlighted segment and the focused row clear. Checked in headless Chromium's emulation only. The PC checks are in `TESTING.md` → "Wheel look and image conversions, M3 first slice", and the shelf's in "File actions, M5".

## M7: Ship Grape for Windows v1 (lane A)
*Status 2026-10-24:* CI builds an unsigned per-user NSIS installer, `Grape-Windows-x64`, on every run (`PLATFORM_WINDOWS.md` → "Running a CI build"), so the PC can test release builds without the dev setup. The tagged release workflow, signing and notices are still to do.
- [ ] GitHub Actions release workflow on tag: NSIS installer (`Grape-Setup-x64.exe`), plus ARM64 and a portable zip if time allows. *Status 2026-10-25:* `release.yml` is written: a `vX.Y.Z` tag checks the versions, builds `Grape-Setup-x64.exe` and `SHA256SUMS.txt`, and drafts the release for Maxwell to publish (`PLATFORM_WINDOWS.md` → "Releasing"). It hasn't run yet; the first real tag tests it (`TESTING.md` → "Release, M7"). x64 only: no ARM64 or portable zip yet.
- [ ] Code signing: apply to SignPath Foundation (free for OSS); ship unsigned until approved and explain the SmartScreen "More info → Run anyway" step in the README.
- [ ] `THIRD_PARTY_NOTICES.md`, `licenses/`, source links for GPL sidecars (ENGINES rule 3). *Status 2026-10-25:* `THIRD_PARTY_NOTICES.md` (the Rust crates and the UI's JavaScript) is generated by cargo-about, checked in CI's Licenses job with cargo-deny, and shipped in both bundles with `LICENSE.txt` (D23). `licenses/` has its rule; the sidecar texts and GPL source links come with D22. *2026-10-26:* Grape builds its own FFmpeg (D24, `scripts/ffmpeg/`). `licenses/ffmpeg/` and the FFmpeg entry in the notices are in for that build, and the build writes the source bundle. Left: CI's build jobs (lane A) and the release carrying the binaries and `grape-ffmpeg-sources.tar`. *2026-10-26, #27:* CI builds them (`ffmpeg.yml`), and every draft release carries `grape-ffmpeg-sources.tar` and grape-heif's source, both in `SHA256SUMS.txt`. Left: Grape's own FFmpeg binaries as release assets, with `scripts/sidecars.lock` pointing at them.
- [ ] Updater (opt-in) pointing to GitHub Releases.

**Accept:** a fresh Windows user can download `Grape-Setup-x64.exe` from GitHub Releases, install without admin rights, and convert a file within a minute.

---

# Phase 2: macOS port

## M8: macOS platform layer
*Started early, 2026-10-23 (DECISIONS D18).* CI builds a downloadable universal DMG on every run (`PLATFORM_MACOS.md` → "Running a CI build"). The platform layer and the app wiring are built and pass CI on macOS. Nothing has run on a real Mac yet: `TESTING.md` → "macOS, first build" lists what to check.
- [x] `grape-platform/macos`: drag pasteboard + modifier polling (`PLATFORM_MACOS.md`), display bounds in points, AppKit file actions (Share is AirDrop), haptics.
- [x] The app on macOS: `ActivationPolicy::Accessory`, the overlay and toast shown without focus, drops through wry, and the two macOS controller rules (D18).
- [ ] Overlay over full-screen apps confirmed on a Mac (`tauri-nspanel` if needed).
- [ ] Engines: ImageIO for HEIC, `h264_videotoolbox` in FFmpeg args. `fetch-sidecars.sh` for macOS builds. *(`fetch-sidecars.sh` is written (D25; M3's sidecar item) and runs in CI's Sidecars job; not run on a Mac by hand yet. ImageIO for HEIC is built too (D33): the HEIC worker is Grape itself, `Grape --grape-heif-worker`; CI's `heif-it-macos` decodes libheif's fixtures with it, and Mac encoding is manual QA. Not run on a Mac yet.)*
- [ ] The full `TESTING.md` QA checklist passes on a Mac.

## M9: Ship Grape for Mac
- [ ] Universal DMG, signed + notarized if a Developer ID is available (DECISIONS D4), otherwise unsigned with README instructions.
- [ ] Release workflow builds Windows + macOS together from then on.

## v2 (after both ship)
Documents, spreadsheets, slides and PDF → office formats, plus all panel/editor tools from `TOOLS_AND_ACTIONS.md`, OCR for scanned PDFs, file promises on macOS, and multi-part RAR archives (D27: piping every volume into bsdtar in order works). Order by user demand.

## Definition of done (every task)
- Tests added or updated; `fmt`, `clippy` and `typecheck` clean.
- Docs updated if behavior changed.
- The summary states which OS it was actually run on.
- No source file modified during the run (QA rule).
