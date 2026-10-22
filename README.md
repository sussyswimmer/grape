<p align="center">
  <img src="docs/assets/grape-header.svg" alt="Grape: drag a file, tap a key, drop it on the wheel" width="100%">
</p>

# Grape

**Drag a file, tap a key, drop on a wheel segment, and the file is converted next to the original, offline.**

Grape is a free, open-source file converter that lives in the tray. Start dragging a file, tap a key, and a wheel opens at the cursor: conversions on one key, tools on another, file actions on a third (on Windows: Shift, Ctrl and Alt by default; you'll be able to remap them). Drop the file on a segment and the result appears next to the original. Your original is never changed.

## Status

**Pre-alpha. Windows first.** Grape is being built for Windows 10 and 11 first; macOS follows once the Windows version ships (see [docs/ROADMAP.md](docs/ROADMAP.md)).

Right now the app is a scaffold: a tray icon with **Settings…**, **Pause Grape** and **Quit**, and a hidden overlay window that the wheel will use. Drag detection and the wheel itself are the next milestones. There is no installer release yet.

## Build and run on Windows

1. Install the tools once, in PowerShell (details in [docs/PLATFORM_WINDOWS.md](docs/PLATFORM_WINDOWS.md) → "Dev setup"):

   ```powershell
   winget install --id Git.Git -e
   winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
   winget install --id Rustlang.Rustup -e
   winget install --id OpenJS.NodeJS.LTS -e
   ```

   Then open a **new** terminal:

   ```powershell
   rustup default stable-msvc
   npm install -g pnpm@10
   ```

   The repo pins its Rust version in `rust-toolchain.toml`; rustup installs it on the first build.

2. Clone the repo to a local drive (not inside OneDrive) and install the UI dependencies:

   ```powershell
   git clone https://github.com/sussyswimmer/grape.git
   cd grape
   pnpm install
   ```

3. Run the app in development mode:

   ```powershell
   cd apps/desktop
   pnpm tauri dev
   ```

   The first build takes several minutes. Grape's icon appears in the notification area; it may be hidden under the **^** overflow arrow. Use its menu to quit.

4. Optional: build an installer with `pnpm tauri build` (also in `apps/desktop`). The NSIS installer lands in `target\release\bundle\nsis\` at the repo root. It installs for the current user, without admin rights. Builds aren't code-signed yet, so Windows SmartScreen may warn about them.

## Tests and checks

From the repo root:

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p grape-core            # fast: matrix, naming, planning
pnpm -C apps/desktop/ui typecheck
pnpm -C apps/desktop/ui test
```

CI runs the same checks on Windows on every push and pull request, and checks that the workspace still compiles on macOS. The manual QA checklist is in [docs/TESTING.md](docs/TESTING.md).

## Offline, always

Grape converts files on your own computer. Nothing is uploaded: no accounts, no telemetry, no remote configuration, and the conversion code never touches the network. The only network feature on the roadmap is an update check, and it will be opt-in.

## How the repo is organized

| Path | What's there |
|---|---|
| `apps/desktop/src-tauri/` | The Tauri 2 app: tray, windows, and (soon) the wheel controller |
| `apps/desktop/ui/` | TypeScript UI for the wheel overlay and the settings window |
| `crates/grape-core/` | The pure model: formats, conversion matrix, tools, output naming, wheel geometry |
| `crates/grape-engines/` | Runs conversions (images, audio and video via FFmpeg, PDF, archives, subtitles) |
| `crates/grape-platform/` | OS glue: drag detection, modifier keys, overlay window styles, file actions |
| `docs/` | The specs. They are the source of truth for how Grape behaves |

New to the code? Start with [docs/START_HERE.md](docs/START_HERE.md) and [CLAUDE.md](CLAUDE.md).

## License

Grape is released under the [MIT License](LICENSE).
