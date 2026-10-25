# Start here: building Grape for Windows with Claude Code

## Where things stand (2026-10-25)

Read this first in a fresh Claude Code session; the rest of the file is the original step-by-step guide.

- **Branches.** Lane A (app, platform, UI, CI) works on `claude/beautiful-ritchie-y3qgbr`. Lane B (engines) works on `claude/vigilant-gauss-l5qt5y`. Each session pushes only its own branch, and merges the other's when it moves. New DECISIONS entries: D19 is lane A's (Settings); lane B uses D20 and up.
- **PRs.** PR #13 (head `claude/vigilant-gauss-l5qt5y`) and PR #14 ("Add files via upload", head `claude/beautiful-ritchie-y3qgbr`) carry the same commits. Lane B fast-forwards #13 to lane A's pushes. #13 has the full description. Maxwell keeps one and closes the other.
- **Try it without the dev setup.** Every CI run uploads `Grape-Windows-x64` (an unsigned per-user NSIS installer, `PLATFORM_WINDOWS.md` → "Running a CI build") and `Grape-macOS-universal` (an ad-hoc-signed DMG, `PLATFORM_MACOS.md` → "Running a CI build") under the run's Artifacts.
- **Built, green in CI, not yet run by hand on the PC or a Mac:** M0, M1 and M2; M3's first slice (20 raster image conversions); M5 Copy, Copy Path, Terminal and Move; the macOS port (D18); M6 Settings (D19: keys with swap, where files go, fallback folder, sounds, haptics, launch at login, terminal app, the first-run welcome card). `TESTING.md` lists every manual check.
- **Next for lane A** (unchecked in `ROADMAP.md`): M5 Share on Windows and the float shelf; M4's tool panels; M7's tagged release workflow, `THIRD_PARTY_NOTICES.md` and `licenses/`, signing and the opt-in updater (needs a DECISIONS entry first: golden rule 3); M3's `scripts/fetch-sidecars.ps1` with lane B.
- **Checking the Mac target from Linux:** `CC_aarch64_apple_darwin=clang AR_aarch64_apple_darwin=llvm-ar-18 CFLAGS_aarch64_apple_darwin="--target=arm64-apple-macos11" cargo clippy --workspace --all-targets --target aarch64-apple-darwin -- -D warnings`. The Windows target: `--target x86_64-pc-windows-msvc`.

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

Then tag a release (`git tag v0.1.0`, `git push --tags`), and the installer appears on the repo's Releases page.

To build an installer locally without CI: `pnpm tauri build` in `apps/desktop`. The installer lands in `apps/desktop/src-tauri/target/release/bundle/nsis/`. If your Cargo workspace puts `target/` at the repo root, look in `target/release/bundle/nsis/` instead.

## Tips
- Keep each Claude Code session to one milestone. Start a fresh session for the next one; `CLAUDE.md` and the docs carry the context.
- If Claude Code and the docs disagree, the docs win. Tell it to update the docs when you change your mind about behavior.
- Mac comes after Windows ships (ROADMAP Phase 2).
