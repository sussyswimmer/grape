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
- [ ] Lane B: engines for images (HEIC via libheif DLLs + `heif-enc` sidecar), audio/video via the FFmpeg sidecar, subtitles, archives, PDF → JPG/PNG/TXT/MD, images → PDF.
- [ ] Sidecars: a script `scripts/fetch-sidecars.ps1` that downloads the pinned FFmpeg/ffprobe/heif-enc builds, checks their SHA-256, and places them in `apps/desktop/src-tauri/binaries/` with target-triple names. Never commit the binaries.
- [ ] Atomic writes, progress events, cancel.
- [ ] Toast: "Making name…" → "Done", with click-to-reveal in Explorer.

**Accept:** every v1 pair (178) converts a fixture file on Windows (`engines-it` test run on the PC and in CI), and spot-checks by hand pass.

## M4: v1 tools (lane B engines, lane A panels)
- [ ] compress (image/audio/video), removeMetadata, muteVideo, normalizeAudio, audioChannels, extractArchive, splitPDF, mergePDF, createPDF.
- [ ] Panels: compress presets, merge reorder list, channels toggle.

## M5: File actions on Windows (lane A)
- [ ] copy (`CF_HDROP`), move (`IFileOperation` with undo), copyPath, terminal (Windows Terminal → PowerShell fallback). See `PLATFORM_WINDOWS.md`.
- [ ] share (Windows share sheet), float shelf (OLE drag-out works).

*Status 2026-10-22:* copy, copyPath, terminal and move are built, with toasts. CI's Windows runner checks the clipboard contents and a real shell move. The picker, the terminals and undo still need the PC (`TESTING.md` → "File actions, M5"). Share and float aren't started.

## M6: Settings + polish (lane A)
- [ ] Key remapping with conflict detection (defaults Shift / Ctrl / Alt). Output location and fallback. Sounds toggle. Launch at login.
- [ ] Pause Grape (tray). First-run onboarding: a 3-step animated "drag, tap, drop" card in Grape's own style.
- [ ] Reduce Motion, light/dark glass following the Windows theme, accessibility labels.

## M7: Ship Grape for Windows v1 (lane A)
- [ ] GitHub Actions release workflow on tag: NSIS installer (`Grape-Setup-x64.exe`), plus ARM64 and a portable zip if time allows.
- [ ] Code signing: apply to SignPath Foundation (free for OSS); ship unsigned until approved and explain the SmartScreen "More info → Run anyway" step in the README.
- [ ] `THIRD_PARTY_NOTICES.md`, `licenses/`, source links for GPL sidecars (ENGINES rule 3).
- [ ] Updater (opt-in) pointing to GitHub Releases.

**Accept:** a fresh Windows user can download `Grape-Setup-x64.exe` from GitHub Releases, install without admin rights, and convert a file within a minute.

---

# Phase 2: macOS port

## M8: macOS platform layer
- [ ] `grape-platform/macos`: drag pasteboard + modifier polling (`PLATFORM_MACOS.md`), AppKit file actions, haptics.
- [ ] Overlay over full-screen apps (`tauri-nspanel` if needed). `ActivationPolicy::Accessory`.
- [ ] Engines: ImageIO for HEIC, `h264_videotoolbox` in FFmpeg args. `fetch-sidecars.sh` for macOS builds.
- [ ] The full `TESTING.md` QA checklist passes on a Mac.

## M9: Ship Grape for Mac
- [ ] Universal DMG, signed + notarized if a Developer ID is available (DECISIONS D4), otherwise unsigned with README instructions.
- [ ] Release workflow builds Windows + macOS together from then on.

## v2 (after both ship)
Documents, spreadsheets, slides and PDF → office formats, plus all panel/editor tools from `TOOLS_AND_ACTIONS.md`, OCR for scanned PDFs, and file promises on macOS. Order by user demand.

## Definition of done (every task)
- Tests added or updated; `fmt`, `clippy` and `typecheck` clean.
- Docs updated if behavior changed.
- The summary states which OS it was actually run on.
- No source file modified during the run (QA rule).
