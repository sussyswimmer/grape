<p align="center">
  <img src="docs/assets/grape-header.svg" alt="Grape: drag a file, tap a key, drop it on the wheel" width="100%">
</p>

# Grape

**Drag a file, tap a key, drop on a wheel segment, and the file is converted next to the original, offline.**

Grape is a free, open-source file converter that lives in the tray. Start dragging a file, tap a key, and a wheel opens at the cursor: conversions on one key, tools on another, file actions on a third (on Windows: Shift, Ctrl and Alt by default; you can remap them in Settings). Drop the file on a segment and the result appears next to the original. Your original is never changed.

## Status

**Pre-alpha. Windows first.** Grape is being built for Windows 10 and 11 first. The macOS port started early and builds in CI, but hasn't run on a Mac yet (see [docs/ROADMAP.md](docs/ROADMAP.md)).

So far, the wheel opens at the cursor when you tap a key while dragging from Explorer or the Desktop. It converts images (JPG, PNG, WEBP, TIFF, BMP, HEIC and AVIF) to each other and to PDF, SVGs to all of those, audio and video, subtitles, archives (RAR too) and PDFs to images or text: all 178 conversions planned for v1. Most v1 tools run too: Compress, Remove Metadata, Remove Audio, Normalize Volume and Audio Channels on the formats they take, and Extract Archive, Split PDF, Merge PDFs and Create PDF. The rest say "Not in this build yet". The six file actions (Copy, Copy Path, Terminal, Move, Share and Float) and Settings work. CI checks all of it on Windows and macOS, and testing by hand on a real PC is next ([docs/TESTING.md](docs/TESTING.md)).

## Install

### Windows

1. Download `Grape-Setup-x64.exe` from the latest release on the [Releases page](https://github.com/sussyswimmer/grape/releases). `SHA256SUMS.txt` next to it lets you check the download; the release notes show how.
2. Run it. It installs for your user only (in `%LOCALAPPDATA%\Grape`), so there's no admin prompt.
3. Grape isn't code-signed yet, so SmartScreen says "Windows protected your PC". Click **More info**, then **Run anyway**.
4. Grape starts in the notification area (it may be under the **^** overflow arrow), and Settings opens with a short welcome card. The first launch also turns on launch at login; you can turn it off in Settings.

To uninstall: Settings → Apps → Installed apps → Grape → Uninstall. Your settings stay in `%APPDATA%\io.github.sussyswimmer.grape\` unless you tick "Delete the application data".

To try a commit that isn't released yet, CI builds the same installer on every run: [docs/PLATFORM_WINDOWS.md](docs/PLATFORM_WINDOWS.md) → "Running a CI build".

### macOS

Released versions come from the [Releases page](https://github.com/sussyswimmer/grape/releases) as `Grape-macOS-universal.dmg`, for macOS 13 Ventura or later on Apple silicon and Intel. No release has it yet: it's published only once it has passed the checks on a real Mac. It's ad-hoc signed and not notarized, so macOS blocks the first open: try to open Grape once, then click **Open Anyway** in System Settings → Privacy & Security. The release notes and [docs/PLATFORM_MACOS.md](docs/PLATFORM_MACOS.md) → "Running a CI build" give each step.

Until then, CI builds the same DMG on every run as a preview, and nobody has tried it on a Mac yet: [docs/PLATFORM_MACOS.md](docs/PLATFORM_MACOS.md) → "Running a CI build".

## Build from source

On Windows:

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

CI runs these checks on Windows on every push and pull request, runs clippy and the Rust tests on macOS, and builds the Windows installer and the Mac DMG. The manual QA checklist is in [docs/TESTING.md](docs/TESTING.md).

## Releasing

A version tag (`vX.Y.Z`) runs the Release workflow. It checks that the tag matches the version in the repo, builds `Grape-Setup-x64.exe` for Windows and `Grape-macOS-universal.dmg` for macOS from the same tag, and makes a draft GitHub Release with both, `SHA256SUMS.txt`, the GPL sources and Grape's own FFmpeg builds. Nothing is published until the draft is checked and published by hand. The steps are in [docs/PLATFORM_WINDOWS.md](docs/PLATFORM_WINDOWS.md) → "Releasing".

## Offline, always

Grape converts files on your own computer. Nothing is uploaded: no accounts, no telemetry, no remote configuration, and the conversion code never touches the network. The only network feature on the roadmap is an update check, and it will be opt-in.

The Windows installer is the one exception, and it isn't Grape: on a PC without Microsoft's WebView2 runtime, which Grape's windows use, it downloads WebView2 first. Windows 11 and current Windows 10 already have it.

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

Grape is released under the [MIT License](LICENSE). The licenses of the code by others that it includes are in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md), which installs with Grape.
