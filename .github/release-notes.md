Grape is a free, open-source file converter for Windows and macOS that lives in the tray, or in the menu bar on a Mac. Drag a file, tap a key, and drop it on the wheel: the converted copy appears next to the original.

## What's new in {{VERSION}}

- TODO: write what's new, then publish.

## Windows

### Install

Grape {{VERSION}} runs on Windows 10 (22H2) and Windows 11, x64.

1. Download **Grape-Setup-x64.exe** from the files below.
2. Run it. It installs for your Windows user only, so it doesn't ask for admin rights.
3. Windows may say **"Windows protected your PC"**. Click **More info**, then **Run anyway**. Grape isn't code-signed yet, so SmartScreen doesn't know it.

Windows 11 and up-to-date Windows 10 already have Microsoft's WebView2 runtime, which Grape's windows use. On a PC without it, the installer downloads it from Microsoft first.

### Check the download

`SHA256SUMS.txt` holds the installer's SHA-256 hash. Download it next to the installer, then run this in PowerShell in that folder:

```powershell
(Get-FileHash .\Grape-Setup-x64.exe).Hash -eq (Get-Content .\SHA256SUMS.txt).Split()[0]
```

It prints `True` when the file is the one built here. If it prints `False`, delete the installer and download it again.

### First run

- Grape starts in the notification area, by the clock. Its icon may be under the **^** arrow.
- Settings opens with a short welcome card: drag, tap, drop. Click **Got it** when you're done.
- Try it on a picture. In File Explorer, start dragging a PNG or JPG, and while you hold the mouse button, tap **Shift**. The wheel opens at the cursor. Drop the file on a format, and the new file appears next to the original.
- **Ctrl** opens the tools and **Alt** the file actions. You can change the keys in Settings. **Esc** closes the wheel.
- Grape turns on launch at login the first time it runs. You can turn it off in Settings.

Your original file is never changed.

### Uninstall

Open Settings → Apps → Installed apps, find **Grape** and choose **Uninstall**. Your settings stay in `%APPDATA%\io.github.sussyswimmer.grape` unless you tick **Delete the application data**.

## macOS

### Install

Grape {{VERSION}} runs on macOS 13 Ventura or later, on Apple silicon and Intel Macs.

1. Download **Grape-macOS-universal.dmg** from the files below.
2. Open it and drag **Grape** into **Applications**.
3. Open Grape from Applications. macOS says it can't check it for malware. Click **Done**. Grape isn't notarized by Apple yet, so macOS blocks its first open.
4. Open **System Settings → Privacy & Security**, scroll to Security, and click **Open Anyway** next to "Grape was blocked". Confirm with your password.
   - Or, in Terminal: `xattr -dr com.apple.quarantine /Applications/Grape.app`, then open Grape normally.

### Check the download

`SHA256SUMS.txt` holds the DMG's SHA-256 hash too. Download it next to the DMG, then run this in Terminal in that folder:

```bash
grep ' Grape-macOS-universal.dmg$' SHA256SUMS.txt | shasum -a 256 -c
```

It prints `Grape-macOS-universal.dmg: OK` when the file is the one built here. If it prints `FAILED`, delete the DMG and download it again.

### First run

- Grape appears in the menu bar. It has no Dock icon. Opening Grape from Applications again opens Settings.
- Settings opens with a short welcome card: drag, tap, drop. Click **Got it** when you're done.
- Try it on a picture. In Finder, start dragging a PNG or JPG, and while you hold the mouse button, tap **Shift**. The wheel opens at the cursor. Drop the file on a format, and the new file appears next to the original.
- **Command** opens the tools and **Control** the file actions. You can change the keys in Settings. **Esc** cancels the drag and closes the wheel.
- Grape doesn't ask for Accessibility or Input Monitoring access.
- Grape turns on launch at login the first time it runs from Applications. You can turn it off in Settings.

Your original file is never changed.

### Uninstall

Turn off launch at login in Grape's Settings, so no login item is left behind. Quit Grape from its menu in the menu bar, then drag **Grape** from Applications to the Trash. Your settings stay in `~/Library/Application Support/io.github.sussyswimmer.grape` and the logs in `~/Library/Logs/io.github.sussyswimmer.grape`. Delete those two folders to remove them too.

## Offline

Everything runs on your computer. Grape uploads nothing, has no account and sends no telemetry, and the conversion code never touches the network. The Windows installer is the only part that may go online: it downloads WebView2 on a PC that doesn't have it. On a Mac, Grape uses the web view built into macOS, so nothing is downloaded.

## Source and licenses

Grape is released under the MIT License. The source for this version is the [v{{VERSION}} tag](https://github.com/sussyswimmer/grape/tree/v{{VERSION}}), and the licenses of the code it includes are in [THIRD_PARTY_NOTICES.md](https://github.com/sussyswimmer/grape/blob/v{{VERSION}}/THIRD_PARTY_NOTICES.md).

The installer also carries programs that Grape runs as separate executables, each built from source for Grape: FFmpeg's `ffmpeg.exe` and `ffprobe.exe` (GPL-2.0-or-later), libarchive's `bsdtar.exe` (BSD-2-Clause) and `grape-heif.exe` (GPL-3.0-or-later). Their license texts are in the `licenses` folder where Grape is installed. The DMG carries the same FFmpeg and bsdtar, built for the Mac, as `ffmpeg`, `ffprobe` and `bsdtar` inside Grape.app, with their license texts in `Grape.app/Contents/Resources/licenses`. On a Mac, Grape itself is the image helper for HEIC and AVIF, so there's no `grape-heif`. The complete source of the two GPL programs is in the files below, `grape-ffmpeg-sources.tar` and `grape-heif-{{HEIF_VERSION}}-source.tar.gz`, and `SHA256SUMS.txt` lists their hashes too.

`grape-ffmpeg-windows-x64.zip` and `grape-ffmpeg-macos-universal.zip` hold Grape's FFmpeg builds as the installer and the DMG carry them, with their license texts. They're for building Grape from source; Grape already has them.
