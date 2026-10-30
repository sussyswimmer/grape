# Start here: building Grape for Windows with Claude Code

## Where things stand (2026-10-28)

Read this first in a fresh Claude Code session; the rest of the file is the original step-by-step guide.

- **Branches.** Lane A (app, platform, UI, CI) works on `claude/beautiful-ritchie-y3qgbr`. Lane B (engines) works on `claude/vigilant-gauss-l5qt5y`. Each session pushes only its own branch, and merges the other's when it moves. DECISIONS numbering: D19, D21, D23 and D25 are lane A's; lane B has D20 and D22. From now on lane B takes even numbers and lane A odd.
- **After merging lane B.** A merge that adds, removes or bumps a dependency can fail CI's Licenses job until the notices are regenerated: run the command at the top of `THIRD_PARTY_NOTICES.md` (it needs the network) and commit the result. A new license (ISC, BSD-2-Clause and so on) goes in both `deny.toml` and `about.toml`.
- **PRs.** PR #13 (head `claude/vigilant-gauss-l5qt5y`) and PR #14 ("Add files via upload", head `claude/beautiful-ritchie-y3qgbr`) carry the same commits. Lane B fast-forwards #13 to lane A's pushes. #13 has the full description. Maxwell keeps one and closes the other.
- **Try it without the dev setup.** Every CI run uploads `Grape-Windows-x64` (an unsigned per-user NSIS installer, `PLATFORM_WINDOWS.md` → "Running a CI build") and `Grape-macOS-universal` (an ad-hoc-signed DMG, `PLATFORM_MACOS.md` → "Running a CI build") under the run's Artifacts. Since 2026-10-28 both carry Grape's own FFmpeg and bsdtar, and the installer grape-heif too.
- **Green in CI on 2026-10-28 (`92cd1b2`, every job):** CI launches the app it built on both OSes before uploading it. On Windows it installs the installer silently, runs the sidecars and Grape's SVG and PDF workers from the install folder, starts Grape, checks setup's log lines and that a second launch hands over, and uninstalls it. On the Mac it runs Grape.app's SVG and PDF workers, then starts the Grape.app the DMG is built from through LaunchServices (its Apple silicon half, with nothing installed), checks its log lines and that a second `open -n` hands over (`TESTING.md` → Layers, "Launch smoke").
- **Green in CI, not yet run by hand on the PC or a Mac** (every job green on 2026-10-28, `fcf4c63`): M0, M1 and M2; M3's conversions: the 20 raster pairs, subtitles, archives (RAR through Grape's bsdtar, D27), PDF → JPG, PNG, TXT and MD (D29), HEIC (D33: `grape-heif.exe` on Windows, ImageIO on macOS) and the 104 audio and video pairs through Grape's own FFmpeg (D22, D24), built in CI for Windows x64 and both Mac architectures and tested there by `engines-it`; M4's Extract Archive, Split PDF, Merge PDFs (with its panel) and Remove Metadata on PDFs; M5's six file actions (Float on the Mac is newer: below); the macOS port (D18); M6 Settings (D19), contrast themes and the welcome card; M7's notices and license check (D23) and `release.yml` (the first real tag tests it). `TESTING.md` lists every manual check.
- **Green in CI on 2026-10-28 (`abf1e3b`), not yet run by hand:** images → PDF and Create PDF (D37): Grape writes the PDF itself, one page at a time, in its own process.
- **Built 2026-10-28, not yet run by hand:** lane B's tools (D26: Compress, Remove Metadata on images and media, Normalize Volume, Audio Channels, Remove Audio), with the app's panels for them and their toasts ("62% smaller", "Removed location and 4 more", "Nothing to change"). CI runs their tests on both OSes (`TESTING.md` → "Media tools, M4", "Image tools, M4", "Tool panels, M4").
- **Green in CI on 2026-10-28 (`6e2a2d8`, every job), not yet run by hand:** the float shelf on the Mac (D43): Control-drop on FLOAT, and items drag out of the shelf as a copy or an alias, never a move. Nothing has run on a Mac (`TESTING.md` → "Float on the Mac, D43", the Dock's Trash first).
- **Green in CI on 2026-10-28 (`6e2a2d8`, every job, the DMG's AVIF at the cap among them), not yet run by hand:** AVIF in and out (D41): 13 pairs through the image helper, now protocol 2 (dav1d and libaom in `grape-heif.exe` on Windows; ImageIO and rav1e on the Mac), so 170 of the 178 v1 pairs run with every helper. Its Windows build and AVIF tests run only in CI. On the PC, replace a `grape-heif.exe` from before D41 in `binaries\` first: it speaks protocol 1, so HEIC drops would fail too (`PLATFORM_WINDOWS.md` → Dev setup).
- **Green in CI on 2026-10-28 (`fef7d4f`, every job), not yet run by hand:** SVG input (D39): the last 8 v1 pairs (SVG → JPG, PNG, WEBP, TIFF, BMP, PDF, and HEIC and AVIF with the image helper) and SVG pages in Create PDF, drawn by resvg in Grape's SVG worker (`Grape --grape-svg-worker`, a process of its own). With every helper, all 178 v1 pairs run. Linked images are left out and counted; text uses the system's fonts (`TESTING.md` → "SVG, M3/M4").
- **Built 2026-10-28; its shared Mac build is green in CI (`fef7d4f`), the release itself waits for the first tag:** a tag's draft release carries the Mac DMG too (D45), as `Grape-macOS-universal.dmg` next to the installer, with Grape's own FFmpeg builds as `grape-ffmpeg-windows-x64.zip` and `grape-ffmpeg-macos-universal.zip`. CI's DMG build moved into `macos-app.yml`, which `ci.yml` and `release.yml` both call. CI's next run tests the move, and the first real tag the rest (`TESTING.md` → "Release, M7" and "Release, M9"). Until the M9 checks pass, the DMG comes off the draft before it's published.
- **Next for lane A** (unchecked in `ROADMAP.md`):
  - M7: Grape's own FFmpeg builds go on every draft release (D45, `grape-ffmpeg-*.zip`; the first real tag tests it); pointing `scripts/sidecars.lock` at a published release's copies is lane B's step. Code signing (SignPath; Maxwell applies first); ARM64 and a portable zip if wanted. The opt-in updater needs Maxwell's decision and a DECISIONS entry first (golden rule 3).
- **Checking the Mac and Windows targets from Linux:** clippy runs build scripts, and libwebp-sys's compiles libwebp's C for the target (D28). Linux has no macOS SDK and no MSVC, so clang compiles it for a MinGW target against mingw-w64's headers (Ubuntu's `mingw-w64` package, `/usr/x86_64-w64-mingw32/include`), only so that the build script finishes: clippy links nothing, and CI's Mac and Windows jobs compile it for real. The Mac target: `CC_aarch64_apple_darwin=clang AR_aarch64_apple_darwin=llvm-ar-18 CFLAGS_aarch64_apple_darwin="--target=aarch64-w64-mingw32 -isystem /usr/x86_64-w64-mingw32/include" cargo clippy --workspace --all-targets --target aarch64-apple-darwin -- -D warnings` (rav1e's arm64 assembly and objc2's exception helper build with it too; the old `--target=arm64-apple-macos11` finds no C library headers for libwebp). The Windows target: `CC_x86_64_pc_windows_msvc=clang AR_x86_64_pc_windows_msvc=llvm-ar-18 CFLAGS_x86_64_pc_windows_msvc="--target=x86_64-w64-mingw32"` with `--target x86_64-pc-windows-msvc` (without them, cc compiles with the host's gcc and stops at `lib.exe`). Never set `TARGET_CPU` (`crates/grape-webp/CLAUDE.md`). The license check: `cargo deny --locked check licenses bans sources` (D23 says how to install it). The fetch scripts' download and hash check: with `GRAPE_SIDECARS_NO_RUN=1` set, run `./scripts/fetch-sidecars.sh`, or the `.ps1` with PowerShell 7 (`pwsh -File`), in a copy of the repo. That skips the `-version` runs and writes no stamp.

A step-by-step path from an empty folder to a Windows installer. Each step has a prompt to paste into Claude Code. Run Claude Code **on the Windows PC**, inside the `grape` folder, so it can build and you can test right away.

## 0. One-time setup
1. Unzip the docs to a normal local folder, e.g. `C:\dev\grape` (not OneDrive).
2. Install the dev tools: `PLATFORM_WINDOWS.md` → "Dev setup".
3. `cd C:\dev\grape`, then `git init`, `git add .` and `git commit -m "Docs: Grape spec"`.
4. Open Claude Code in that folder. It loads `CLAUDE.md` automatically.

Optional: create an empty GitHub repo and push it, so CI (M0) can build the installer later.

## 1. M0: Scaffold
> Read CLAUDE.md, docs/ROADMAP.md (M0) and docs/ARCHITECTURE.md. Plan M0 for Windows: the Cargo workspace with grape-core, grape-engines and grape-platform (with a compiling macOS stub), the Tauri 2 app in apps/desktop with a TS + Vite UI, a tray icon with Settings/Pause/Quit, the hidden overlay window, and a GitHub Actions CI on windows-latest. Show me the plan first, then build it, and run `pnpm tauri dev` to prove the tray icon works.

Check: Grape's icon appears in the system tray (it may sit under the ^ overflow arrow), and Quit closes it.

## 2. M2: Core model (Codex can take this in parallel)
> Implement M2 in crates/grape-core exactly per docs/FORMAT_MATRIX.md and docs/INTERACTION_SPEC.md §4, following crates/grape-core/CLAUDE.md. Encode the matrix as data. Add every test listed under "Test assertions", including 275 total and 178 v1 pairs, and hit-test property tests. Run cargo test -p grape-core.

## 3. M1: Drag + overlay spike ⚠️ the hardest step
> Do M1 from docs/ROADMAP.md on Windows, following docs/PLATFORM_WINDOWS.md and crates/grape-platform/CLAUDE.md. Use plan mode. Implement the WH_MOUSE_LL hook on its own message-pumping thread, GetAsyncKeyState polling, and the Explorer/Desktop allowlist; then open the overlay on the right monitor with a placeholder ring, and log DragDrop Enter/Over/Drop with hit-test results. Then walk me through testing it by hand.

Test it yourself:
- Drag a file in Explorer, tap Shift → a ring appears at the cursor.
- Drop on the top segment → the log shows the file path.
- Drop outside the ring → nothing happens.
- Press Esc mid-drag → the ring hides, and you can still drop into a folder normally.
- **Shift-drop 10 times → the original file is still there every time.** If it ever vanishes, stop and report it. That's golden rule 2.

## 4. M3–M6: The real app
Use `/next-task A` (Claude Code) and `/next-task B` (Codex) to work through the roadmap in order. Before each milestone:
> Run /next-task A

After each milestone:
> Run /qa-drag

`/qa-drag` uses scratch copies of the test fixtures, never your real files.

## 5. M7: Downloadable installer
> Do M7 for Windows: a GitHub Actions release workflow that builds the NSIS installer Grape-Setup-x64.exe on a version tag and attaches it to a GitHub Release, with sidecars fetched by scripts/fetch-sidecars.ps1, THIRD_PARTY_NOTICES.md and licenses/. Update the README with install steps, including the SmartScreen "More info → Run anyway" note for unsigned builds.

Then tag a release (`git tag v0.1.0`, `git push origin v0.1.0`). The installer appears in a draft on the repo's Releases page; check it and publish it (`PLATFORM_WINDOWS.md` → "Releasing").

To build an installer locally without CI: `pnpm tauri build` in `apps/desktop`. The installer lands in `apps/desktop/src-tauri/target/release/bundle/nsis/`. If your Cargo workspace puts `target/` at the repo root, look in `target/release/bundle/nsis/` instead.

## Tips
- Keep each Claude Code session to one milestone. Start a fresh session for the next one; `CLAUDE.md` and the docs carry the context.
- If Claude Code and the docs disagree, the docs win. Tell it to update the docs when you change your mind about behavior.
- Mac comes after Windows ships (ROADMAP Phase 2).
