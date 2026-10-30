# Grape

Grape is a zero-click, offline file converter for **macOS and Windows**. The user starts dragging a file, taps a modifier key, and a radial wheel appears at the cursor. Dropping the file on a segment converts it, runs a tool on it, or performs a file action. Everything runs locally and nothing is uploaded. Grape is **free and open source**.

Grape's behavior is modeled on Tangerine for Mac (tangerineformac.com). The behavior spec was recovered with REA from Tangerine's public website demo. Grape's own code, design and assets are original. Read `docs/REA_FINDINGS.md` to see what was observed versus decided.

## Current phase: Windows first

We are in **Phase 1: Windows** (`docs/ROADMAP.md`, DECISIONS D8). Development and testing happen on Maxwell's Windows PC.
- Since 2026-10-23 the macOS port (M8) runs alongside it (DECISIONS D18). Mac code lives in `grape-platform`'s `macos` module and Mac-gated app code. CI builds a downloadable DMG, and nothing on the Mac is tested by hand until Maxwell runs that build.
- Keep shared code (`grape-core`, `grape-engines`, the UI) OS-neutral.
- New here? Read `docs/START_HERE.md` first.

## Golden rules (never break these)

1. **Clean room.** Do not copy Tangerine's code, icons, sounds, images, wording, colors or name. Their demo was read only to learn *behavior*. If a task seems to need their assets, stop and ask.
2. **Originals are sacred.** Never modify, overwrite, move or delete a source file. The only exception is the explicit **Move** file action. Outputs get new names (see `docs/FORMAT_MATRIX.md` → Output naming).
3. **Offline only.** No network calls in the conversion path, ever. No telemetry and no remote config. The only possible exception is an update check, and that needs an entry in `docs/DECISIONS.md` first.
4. **Never break the user's drag.** If Grape can't show the wheel, it must get out of the way. The OS drag continues as if Grape weren't installed. Esc always closes the wheel.
5. **One behavior spec, two platforms.** The wheel, matrix, naming and tools are shared. Only the drag detection, overlay window and file actions are platform-specific.
6. **No GPL code inside Grape's own binaries.** Grape is free and open source (`docs/DECISIONS.md` D2). GPL tools (FFmpeg with x264, x265 in `grape-heif`, pandoc) may ship **only as separate sidecar executables**, with license texts and source links. Everything we link must be permissive, or LGPL linked dynamically. See `docs/ENGINES.md` → Licensing rules.

## Stack

- **App shell:** Tauri 2. It runs as a tray/menu-bar app with no dock or taskbar icon, and has a settings window plus a transparent overlay window for the wheel.
- **Core logic:** Rust in the `crates/` workspace. It is pure and unit-testable, with no UI and no OS calls.
- **UI:** TypeScript (strict), Vite and vanilla DOM/SVG in `apps/desktop/ui`. No framework is needed for the wheel. Use one only for the settings window if it earns its place.
- **Engines:** bundled sidecar binaries (FFmpeg, ffprobe, grape-heif on Windows) plus Rust crates and OS frameworks. See `docs/ENGINES.md`.

## Repo layout (target)

```
apps/desktop/src-tauri/   Tauri app: windows, tray, commands, event wiring  (has CLAUDE.md)
apps/desktop/ui/          Wheel overlay + settings UI                       (has CLAUDE.md)
crates/grape-core/        Format registry, matrix, tools, naming, job plans (has CLAUDE.md)
crates/grape-engines/     Executes job plans: FFmpeg sidecar, image, PDF... (has CLAUDE.md)
crates/grape-heif-proto/  The image helper protocol (HEIC, AVIF): pure, std only (D33, D41) (has CLAUDE.md)
crates/grape-platform/    Drag detection, modifiers, file actions per OS    (has CLAUDE.md)
crates/grape-webp/        libwebp's encoder for WebP Compress: the only unsafe for images (D28) (has CLAUDE.md)
tools/grape-heif/         grape-heif.exe, the Windows image helper (HEIC, AVIF): its own workspace, GPL as built (D33, D41)
docs/                     Specs — the source of truth for behavior
.claude/commands/         Slash commands for recurring tasks
```

Each nested `CLAUDE.md` holds the rules for that area. Read it when you work there.

## Commands

These exist once milestone M0 is scaffolded. Keep this list current. They work in PowerShell on Windows. Machine setup is in `docs/PLATFORM_WINDOWS.md` → "Dev setup".

```bash
pnpm install                      # UI deps
pnpm tauri dev                    # run the app (from apps/desktop)
cargo test --workspace            # all Rust tests
cargo test -p grape-core          # fast: matrix, naming, planning
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
pnpm -C apps/desktop/ui typecheck
pnpm -C apps/desktop/ui test
pnpm tauri build                  # local release build for the current OS
.\scripts\fetch-sidecars.ps1      # Windows: pinned FFmpeg/ffprobe and PDFium into src-tauri/binaries/ (D25, D29; -Force refetches, -Only pdfium fetches one)
./scripts/fetch-sidecars.sh       # macOS: the same, universal (--force refetches, --only pdfium fetches one)
cargo test -p grape-engines --features pdf-it   # PDF tests that load PDFium: set GRAPE_TEST_PDFIUM to the fetched library (D29)
.\scripts\bsdtar\build.ps1 -Install     # Windows: Grape's bsdtar for RAR (D27) into src-tauri/binaries/
./scripts/bsdtar/build.sh --install      # macOS: the same, universal
cargo test -p grape-engines --features rar-it --lib archive::rar   # RAR tests with the real bsdtar: set GRAPE_TEST_BSDTAR
.\tools\grape-heif\build.ps1 -Install   # Windows: Grape's image helper for HEIC and AVIF, grape-heif.exe (D33, D41), into src-tauri/binaries/ (about 45-50 min and 5 GB the first time)
cargo test -p grape-engines --features heif-it   # HEIC and AVIF with the real helper: set GRAPE_TEST_HEIF and GRAPE_TEST_HEIF_FIXTURES (D33, D41)
cargo test -p grape-platform --features avif-encode   # the Mac worker's AVIF encoder (rav1e), on any OS (D41)
cargo deny --locked check licenses bans sources   # license check (D23)
cargo about generate --locked --fail --manifest-path apps/desktop/src-tauri/Cargo.toml -c about.toml -o THIRD_PARTY_NOTICES.md about.hbs   # after a dependency change
```

## Which doc to read

| Before you…                                     | Read                         |
|-------------------------------------------------|------------------------------|
| touch the wheel, keys, drag/drop flow           | `docs/INTERACTION_SPEC.md`   |
| add/change a conversion or output name          | `docs/FORMAT_MATRIX.md`      |
| add/change a tool or file action                | `docs/TOOLS_AND_ACTIONS.md`  |
| change crate boundaries, windows, IPC           | `docs/ARCHITECTURE.md`       |
| write macOS-specific code                       | `docs/PLATFORM_MACOS.md`     |
| write Windows-specific code                     | `docs/PLATFORM_WINDOWS.md`   |
| pick or bundle a codec/library                  | `docs/ENGINES.md`            |
| start a milestone or choose the next task       | `docs/ROADMAP.md`            |
| make a choice that's hard to reverse            | `docs/DECISIONS.md`          |
| write or run tests / manual QA                  | `docs/TESTING.md`            |
| wonder "how does Tangerine actually do X?"      | `docs/REA_FINDINGS.md`       |
| need the product overview                       | `docs/PRODUCT.md`            |
| are starting fresh on this repo                 | `docs/START_HERE.md`         |

## How to work in this repo

- **Spec first.** If code and docs disagree, the docs win unless the user says otherwise. When behavior changes on purpose, update the doc in the same change.
- **Plan before platform code.** Drag detection, overlay windows and hooks are the riskiest parts. Use plan mode, keep each change small, and say which OS you could actually test on.
- **Keep core pure.** `grape-core` must not depend on Tauri, FFmpeg or any OS API. If you need one there, the code belongs in another crate.
- **New dependency → note it.** Add a line to `docs/DECISIONS.md` with the crate or binary, its license, and why it's needed.
- **Tests with every matrix/naming change.** The matrix test asserts exactly 275 conversion pairs for full parity. See `docs/TESTING.md`.
- **Don't guess platform behavior.** If you can't run on that OS, write the code behind a feature flag, add a manual QA item, and say so in your summary.
- **Rust:** edition 2021, `thiserror` for library errors, `anyhow` only in the app crate, no `unwrap()` outside tests.
- **TS:** `strict: true`, no `any`, and every DOM query must be null-checked.
- **Commits:** small, imperative subject, with one concern per commit.

## Platform key defaults

| Wheel        | macOS    | Windows |
|--------------|----------|---------|
| Conversions  | Shift    | Shift   |
| Tools        | Command  | Ctrl    |
| File actions | Control  | Alt     |

Keys are user-remappable in Settings. See `docs/INTERACTION_SPEC.md`.
