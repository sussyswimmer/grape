Grape is a free, open-source file converter for Windows that lives in the tray. Drag a file, tap a key, and drop it on the wheel: the converted copy appears next to the original.

## What's new in {{VERSION}}

- TODO: write what's new, then publish.

## Install

Grape {{VERSION}} runs on Windows 10 (22H2) and Windows 11, x64.

1. Download **Grape-Setup-x64.exe** from the files below.
2. Run it. It installs for your Windows user only, so it doesn't ask for admin rights.
3. Windows may say **"Windows protected your PC"**. Click **More info**, then **Run anyway**. Grape isn't code-signed yet, so SmartScreen doesn't know it.

Windows 11 and up-to-date Windows 10 already have Microsoft's WebView2 runtime, which Grape's windows use. On a PC without it, the installer downloads it from Microsoft first.

## Check the download

`SHA256SUMS.txt` holds the installer's SHA-256 hash. Download it next to the installer, then run this in PowerShell in that folder:

```powershell
(Get-FileHash .\Grape-Setup-x64.exe).Hash -eq (Get-Content .\SHA256SUMS.txt).Split()[0]
```

It prints `True` when the file is the one built here. If it prints `False`, delete the installer and download it again.

## First run

- Grape starts in the notification area, by the clock. Its icon may be under the **^** arrow.
- Settings opens with a short welcome card: drag, tap, drop. Click **Got it** when you're done.
- Try it on a picture. In File Explorer, start dragging a PNG or JPG, and while you hold the mouse button, tap **Shift**. The wheel opens at the cursor. Drop the file on a format, and the new file appears next to the original.
- **Ctrl** opens the tools and **Alt** the file actions. You can change the keys in Settings. **Esc** closes the wheel.
- Grape turns on launch at login the first time it runs. You can turn it off in Settings.

Your original file is never changed.

## Uninstall

Open Settings → Apps → Installed apps, find **Grape** and choose **Uninstall**. Your settings stay in `%APPDATA%\io.github.sussyswimmer.grape` unless you tick **Delete the application data**.

## Offline

Everything runs on your PC. Grape uploads nothing, has no account and sends no telemetry, and the conversion code never touches the network. The installer is the only part that may go online: it downloads WebView2 on a PC that doesn't have it.

## Source and licenses

Grape is released under the MIT License. The source for this version is the [v{{VERSION}} tag](https://github.com/sussyswimmer/grape/tree/v{{VERSION}}), and the licenses of the code it includes are in [THIRD_PARTY_NOTICES.md](https://github.com/sussyswimmer/grape/blob/v{{VERSION}}/THIRD_PARTY_NOTICES.md).
