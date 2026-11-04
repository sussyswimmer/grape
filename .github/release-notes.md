<p align="center">
  <b>Drag a file. Tap a key. Drop it on the wheel.</b><br>
  The converted copy appears next to the original. Nothing is uploaded.
</p>

<p align="center">
  <a href="https://github.com/sussyswimmer/grape/releases/latest"><img src="https://img.shields.io/github/v/release/sussyswimmer/grape?label=release&color=6b4ea2" alt="Latest release"></a>
  <img src="https://img.shields.io/badge/Windows-10%2022H2%20%7C%2011-0078d4" alt="Windows 10 22H2 or Windows 11">
  <img src="https://img.shields.io/badge/macOS-13%20Ventura%2B-1d1d1f" alt="macOS 13 Ventura or later">
  <a href="https://github.com/sussyswimmer/grape/blob/v{{VERSION}}/LICENSE"><img src="https://img.shields.io/github/license/sussyswimmer/grape?color=3da639" alt="MIT License"></a>
  <img src="https://img.shields.io/badge/network-none-critical" alt="Makes no network calls">
</p>

---

Grape is a free, open-source file converter that sits in your tray and waits for a drag. Start dragging a file, tap a key, and a wheel opens at the cursor: conversions on one key, tools on another, file actions on a third. Drop the file on a segment and the converted copy lands next to the original.

Everything runs on your computer. No account, no telemetry, and the conversion path never touches the network.

## What's new in {{VERSION}}

<!-- Rewrite this section for each release. Keep it short: what changed, not what Grape is. -->

**The first tagged build of Grape.** This is pre-alpha software, published so the wheel can be tried on real machines.

- **All 178 conversions planned for v1 are in.** 45 input formats and 275 pairs in the full matrix; v1 covers the 178 that matter most.
- **Images** — JPG, PNG, WebP, TIFF, BMP, HEIC and AVIF convert between each other and to PDF. SVG converts to all of them.
- **Audio and video** — 104 pairs, converted by Grape's own FFmpeg build, which ships inside the installer and the DMG.
- **Subtitles** — SRT and VTT to each other and to plain text.
- **Archives** — ZIP, TAR and GZIP between each other, and RAR in, through Grape's own bsdtar.
- **PDF** — to JPG, PNG, TXT and MD.
- **9 of the 26 tools** — Compress, Remove Metadata, Remove Audio, Normalize Volume, Audio Channels, Extract Archive, Split PDF, Merge PDFs and Create PDF. The others say "Not in this build yet".
- **All six file actions** — Copy, Copy Path, Terminal, Move, Share and Float, plus Settings: remappable keys, output folder, sounds, haptics on a Mac, and launch at login.

## Files in this release

| File | What it's for |
|---|---|
| **`Grape-Setup-x64.exe`** | The Windows installer. Windows 10 22H2 and 11, x64. Installs for your user only, no admin rights. |
| **`Grape-macOS-universal.dmg`** | The Mac app. macOS 13 Ventura or later, Apple silicon and Intel. **Preview: see [Status](#status).** |
| **`SHA256SUMS.txt`** | The SHA-256 hash of every file on this release, for checking what you downloaded. |
| `grape-ffmpeg-sources.tar` | Complete source of the FFmpeg Grape ships (GPL-2.0-or-later), as the license requires. |
| `grape-heif-{{HEIF_VERSION}}-source.tar.gz` | Complete source of `grape-heif.exe`, Grape's Windows image helper (GPL-3.0-or-later). |
| `grape-ffmpeg-windows-x64.zip`<br>`grape-ffmpeg-macos-universal.zip` | Grape's own FFmpeg builds, for building Grape from source. Grape already carries them. |

## Install on Windows

1. Download **`Grape-Setup-x64.exe`** from the files above.
2. Run it. It installs into `%LOCALAPPDATA%\Grape` for your user only, so there's no admin prompt.
3. Windows may say **"Windows protected your PC"**. Click **More info**, then **Run anyway** — Grape isn't code-signed yet, so SmartScreen has never seen it.
4. Grape starts in the notification area, by the clock. Its icon may be hiding under the **^** arrow.

Windows 11 and up-to-date Windows 10 already have Microsoft's WebView2 runtime, which Grape's windows use. On a PC without it, the installer fetches it from Microsoft first — that is the only network call in the whole install.

## Install on macOS

1. Download **`Grape-macOS-universal.dmg`** from the files above.
2. Open it and drag **Grape** into **Applications**.
3. The first time you open Grape, macOS refuses: Grape isn't notarized by Apple. Close the message with **Done**, **OK** or **Cancel** — not **Move to Trash**.
4. Open **System Settings → Privacy & Security**, scroll to Security, and click **Open Anyway** next to "Grape was blocked". Confirm, and enter your password if macOS asks.
   - Or, in Terminal: `xattr -dr com.apple.quarantine /Applications/Grape.app`, then open Grape normally.
5. Grape appears in the menu bar. It has no Dock icon.

## Check your download

Both commands below use the file you downloaded and `SHA256SUMS.txt` from this release, in the same folder. They are worth the ten seconds: a truncated installer is the most common reason an install fails.

**Windows (PowerShell):**

```powershell
(Get-FileHash .\Grape-Setup-x64.exe).Hash -eq (Get-Content .\SHA256SUMS.txt).Split()[0]
```

It prints `True` when the file is the one built here. `False` means download it again.

**macOS (Terminal):**

```bash
grep ' Grape-macOS-universal.dmg$' SHA256SUMS.txt | shasum -a 256 -c
```

It prints `Grape-macOS-universal.dmg: OK`. `FAILED` means download it again.

## First run

Grape shows a three-step welcome card the first time: drag, tap, drop. Then try it on any picture: start dragging it in Explorer or Finder, and while you hold the mouse button, tap the key for conversions. The wheel opens at the cursor. Drop the file on a format and the new file appears next to the original.

| You want | Windows | macOS |
|---|---|---|
| Conversions | **Shift** | **Shift** |
| Tools | **Ctrl** | **Command** |
| File actions | **Alt** | **Control** |
| Close the wheel | **Esc** | **Esc** |

Every key is remappable in Settings, where you also choose the output folder, turn sounds and haptics on or off, and turn launch at login off (Grape turns it on the first time it runs).

**Your original file is never changed.** Outputs get new names next to it, and the only action that can move a source is Move, which asks for a folder first.

## Status

**Pre-alpha, Windows first.** This is the first release, and it is honest about that:

- **CI builds and launches it on both operating systems** — on Windows it installs the installer, starts Grape and uninstalls it; on a Mac it starts the app the DMG is built from. Everything else in the checklist was checked by automated tests, not by a person dragging a file.
- **The macOS DMG is a preview.** It is CI-built, ad-hoc signed and universal, and it hasn't been through the by-hand Mac checklist yet. If you try it, you are the test — please report what breaks.
- **Neither build is code-signed**, hence SmartScreen on Windows and **Open Anyway** on macOS.
- **Windows is x64 only.** No ARM64 build and no portable zip yet.
- **No updater yet.** Watch this repo, or the releases feed, for new versions.
- **17 of the 26 tools are not built.** They show "Not in this build yet" on the wheel instead of failing halfway.

## Uninstall

**Windows:** Settings → Apps → Installed apps → **Grape** → Uninstall. Your settings stay in `%APPDATA%\io.github.sussyswimmer.grape` unless you tick **Delete the application data**.

**macOS:** turn off launch at login in Grape's Settings first, quit Grape from the menu bar, then drag **Grape** from Applications to the Trash. Settings live in `~/Library/Application Support/io.github.sussyswimmer.grape` and logs in `~/Library/Logs/io.github.sussyswimmer.grape`; delete those two folders to remove them as well.

## Offline, always

Grape uploads nothing. There is no account, no telemetry, no remote configuration, and the conversion code never touches the network. The Windows installer is the only part that may go online: it downloads WebView2 on a PC that doesn't have it. On a Mac, Grape uses the web view built into macOS, so nothing is downloaded.

## Source and licenses

Grape is released under the [MIT License](https://github.com/sussyswimmer/grape/blob/v{{VERSION}}/LICENSE). The source for this version is the [v{{VERSION}} tag](https://github.com/sussyswimmer/grape/tree/v{{VERSION}}), and the licenses of the code it includes are in [THIRD_PARTY_NOTICES.md](https://github.com/sussyswimmer/grape/blob/v{{VERSION}}/THIRD_PARTY_NOTICES.md).

The installer also carries programs that Grape runs as separate executables, each built from source for Grape: FFmpeg's `ffmpeg.exe` and `ffprobe.exe` (GPL-2.0-or-later), libarchive's `bsdtar.exe` (BSD-2-Clause) and `grape-heif.exe` (GPL-3.0-or-later). Their license texts are in the `licenses` folder where Grape is installed. The DMG carries the same FFmpeg and bsdtar, built for the Mac, as `ffmpeg`, `ffprobe` and `bsdtar` inside Grape.app, with their license texts in `Grape.app/Contents/Resources/licenses`. On a Mac, Grape itself is the image helper for HEIC and AVIF, so there's no `grape-heif`. The complete source of the two GPL programs is in the files below, `grape-ffmpeg-sources.tar` and `grape-heif-{{HEIF_VERSION}}-source.tar.gz`, and `SHA256SUMS.txt` lists their hashes too.

`grape-ffmpeg-windows-x64.zip` and `grape-ffmpeg-macos-universal.zip` hold Grape's own FFmpeg builds for Windows and the Mac, with their license texts. They're for building Grape from source; Grape already has them.

---

<p align="center">
  <sub>Behavior specs, architecture and the QA checklist live in <a href="https://github.com/sussyswimmer/grape/tree/v{{VERSION}}/docs">docs/</a>.
  Bugs and ideas: <a href="https://github.com/sussyswimmer/grape/issues">open an issue</a>.</sub>
</p>
