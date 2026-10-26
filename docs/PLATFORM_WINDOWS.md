# Windows platform notes

Target: Windows 10 22H2+ and Windows 11, x64 and ARM64. WebView2 is preinstalled on 11. On 10, Tauri's bootstrapper installs it. Rust bindings: the `windows` crate.

## Dev setup (on the Windows PC)

Install these once, in PowerShell:

```powershell
winget install --id Git.Git -e
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install --id Rustlang.Rustup -e
winget install --id OpenJS.NodeJS.LTS -e
```

Then open a **new** terminal and run:

```powershell
rustup default stable-msvc
rustup target add aarch64-pc-windows-msvc   # only if building ARM64 too
npm install -g pnpm@10
```

- WebView2 is built into Windows 11. On Windows 10, install the Evergreen runtime if `pnpm tauri dev` asks for it.
- Check the setup with `rustc -vV` (host should end in `windows-msvc`), `node -v` and `pnpm -v`.
- Keep the repo on a local drive, **not** inside OneDrive. OneDrive syncing `target/` and `node_modules/` makes builds slow and flaky.
- The first `cargo build` takes several minutes, because Tauri has many dependencies. Later builds are incremental.
- Windows Defender can slow builds a lot. Optionally add the repo folder as an exclusion in Windows Security → Virus & threat protection → Exclusions.

## Running a CI build

To try Grape without the dev setup above, install the build CI makes on every run. It's a release build, unsigned until M7.

A released version comes from GitHub Releases instead, as `Grape-Setup-x64.exe` (README → "Install", and "Releasing" below). The steps are the same. A CI build is for a commit that isn't released yet.

1. Open the run on GitHub (Actions → CI → the run for the commit). Under **Artifacts**, download `Grape-Windows-x64` and unzip it.
2. Run `Grape_<version>_x64-setup.exe`. It installs for your user only, so no admin prompt.
3. SmartScreen says "Windows protected your PC". Click **More info**, then **Run anyway**.
4. Grape starts in the notification area (it may be under the ^ overflow arrow). As a release build, its first launch also turns on launch at login (D19); turn that off in Settings if you don't want it.
5. To remove it: Settings → Apps → Installed apps → Grape → Uninstall. Your settings stay in `%APPDATA%\io.github.sussyswimmer.grape\`.

The installer is made by Tauri's NSIS bundler. If WebView2 is missing (rare on Windows 10, never on Windows 11), the installer fetches it; Grape itself still makes no network calls.

## Detecting a file drag

Windows has no system-wide "drag in progress" signal, and the dragged data object is only visible to a window the cursor enters. So detection is a **heuristic**, and the overlay is what learns the file list.

1. **Low-level mouse hook** (`SetWindowsHookExW(WH_MOUSE_LL)`) on a dedicated thread **that pumps messages** (`GetMessageW` loop). A hook thread that doesn't pump freezes the whole system's mouse.
   - `WM_LBUTTONDOWN` records the start point.
   - `WM_MOUSEMOVE` with the button down past `SM_CXDRAG`/`SM_CYDRAG` → candidate drag.
   - `WM_LBUTTONUP` → `Ended`.
   - Keep the hook callback tiny: post to a channel and return `CallNextHookEx` immediately. Windows silently removes hooks that exceed `LowLevelHooksTimeout`.
   - **[Grape, M1]** The callback only forwards presses, releases and moves-while-pressed to a worker thread. The worker does everything slower: the allowlist check below (`WindowFromPoint` can wait on another app's window), the drag threshold and the key polling. A missed release is caught by polling the mouse buttons, after three "up" readings in a row.
2. **Modifiers:** `GetAsyncKeyState(VK_SHIFT / VK_CONTROL / VK_MENU)` polled at ~120 Hz while a candidate drag is live. This avoids a keyboard hook entirely. Add `WH_KEYBOARD_LL` only if polling misses fast taps in testing.
3. **Is it a *file* drag?** Unknown until the overlay gets `DragDropEvent::Enter { paths }`.
   - **[Grape]** Reduce false positives: only arm when the window under the cursor at mouse-down (`WindowFromPoint` → `GetAncestor(GA_ROOT)` → class name) is Explorer (`CabinetWClass`, `ExploreWClass`) or the desktop (`Progman`, `WorkerW`), **or** the user enabled "Any app" in Settings.
   - If the overlay shows but `Enter` brings no paths within 300 ms of the next cursor movement, hide it.
4. **Tap trigger:** in Explorer, holding Shift while dropping switches the drag to *move* and Ctrl to *copy*. Grape opens on the key going **down**. The drop then happens on Grape's overlay, so Explorer's effect choice no longer matters (see the next section). A key already held when the drag starts doesn't open a wheel (DECISIONS D14).
5. **Esc** is polled with `GetAsyncKeyState` too. Explorer cancels its own drag when Esc is pressed (OLE's `QueryContinueDrag`), so on Windows Esc hides the wheel *and* ends the drag.

## The drop must never move the source

Explorer's source-side behaviour depends on the drop effect the target reports. With Shift held, the user's intent looks like "move". If a target returns `DROPEFFECT_MOVE` and doesn't handle the move itself, Explorer can **delete the source** ("optimized move" / `CFSTR_PERFORMEDDROPEFFECT`).

- **M1 finding:** wry 0.57's drop handler returns `DROPEFFECT_COPY` from `DragEnter` and `DragOver`, but never sets the effect in `Drop`, and leaves it unset for drags without files. So Grape registers its own `IDropTarget` on the overlay HWND and all its child windows (`grape_platform::windows::install_drop_target`, DECISIONS D13). It reports `DROPEFFECT_COPY` for file drags and `DROPEFFECT_NONE` otherwise, in every callback. CI checks this with the shell's own data object and Shift, Ctrl and Alt held. The manual 10× test on the PC still applies.
- **The drag-source side (the float shelf, D21).** When Grape is the source, it never offers a move. A drag out of the shelf allows `DROPEFFECT_COPY | DROPEFFECT_LINK` only, so OLE turns a target's move into nothing, and a CI test checks that the allowed effects have no move bit. The data object is Grape's own `IDataObject`: it serves `CF_HDROP` and `Preferred DropEffect` = copy itself, and refuses the shell ID list formats (`CFSTR_SHELLIDLIST` and `CFSTR_SHELLIDLISTOFFSET`) as a second guard. A CI test checks that `QueryGetData` for `CFSTR_SHELLIDLIST` fails. An empty `SHCreateDataObject` with those two formats set on it won't do: on real Windows it answers `CFSTR_SHELLIDLIST` anyway, as CI showed. ReactOS's clean-room Recycle Bin target deletes the files of any drop with a shell ID list itself, in `Drop`, without looking at the allowed effects, and with Shift held the delete is permanent. Windows' own OLE layer and Recycle Bin are closed source, so dropping a shelf item on the Recycle Bin, plain and with Shift, is the first Float check on the PC.
- This is golden rule 2, and it's the single most important Windows test.

## Overlay window
- Tauri window as described in `ARCHITECTURE.md`, plus `WS_EX_TOOLWINDOW` (no taskbar or Alt-Tab), `WS_EX_NOACTIVATE`, and `HWND_TOPMOST`.
- Size it to the monitor under the cursor using `MonitorFromPoint` + `GetMonitorInfoW` (`rcWork` for clamping, `rcMonitor` for size).
- Per-monitor DPI v2: Tauri already declares it. Convert hook coordinates (physical) consistently and test mixed 100%/150% setups.
- **UIPI:** drags from an *elevated* app (e.g. an admin Explorer window) can't drop onto a non-elevated Grape. That's expected; document it and don't try to run Grape elevated.

## Sound
No haptics on Windows. The tick plays via WebAudio in the overlay or `PlaySoundW` with `SND_ASYNC | SND_MEMORY` on a preloaded WAV.

## File actions
- Share: the share sheet, through `IDataTransferManagerInterop::ShowShareUIForWindow` (D21). Nearby Share appears in it if the user turned it on.
  - Files only. Folders, missing items and shortcuts (`.lnk`, `.url`, `.wsh`, which `StorageFile` can't represent) are left out, and a toast says so. A `\\?\` prefix is stripped first.
  - On the action's thread, in the multithreaded apartment, each file is looked up with `StorageFile::GetFileFromPathAsync`, all at once, with 10 s for the lot. A file that fails or is late is left out too. Each one is kept as an agile reference so it can cross threads. Grape keeps the multithreaded apartment for the app's life (`CoIncrementMTAUsage`, once), so it isn't torn down under the files the sheet's package still holds.
  - The sheet belongs to an anchor window on a thread of its own, "grape-share": a single-threaded apartment with a message loop, started on first use and kept for the app's life. The anchor is a 32 × 32 logical px `WS_POPUP` centered where the wheel was, `WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT` at alpha 1 of 255: no taskbar button or Alt-Tab entry, invisible and click-through. It isn't topmost and isn't `WS_EX_NOACTIVATE`, and it's shown with `SW_SHOWNOACTIVATE`, so Grape never takes the focus to open the sheet.
  - The `DataRequested` handler sets a title (the file's name, or "{n} files"; never empty), `DataPackageOperation::Copy` and the files read-only (`SetStorageItems(…, true)`). It calls `FailWithDisplayText` if any of that fails, and always returns `Ok`. It's removed as Chromium does it: right after the call when it already ran inside it or the call failed, otherwise at the start of the handler, and never inside a synchronous call.
  - If the handler hasn't run 30 s after the call (Chromium's limit), Share fails with a "Couldn't share" toast, and the handler is removed.
  - When the sheet closes and activates the anchor (once it has asked for the files, whether they filled or not), the anchor hands the foreground back to the window that had it when the share began, unless that one is gone or Grape's, and then hides. It hands it back first, while the anchor is still the active window, so Grape may still set the foreground.
  - Every HRESULT on the way goes to the log (`share: …`), because which windows the sheet accepts isn't documented anywhere.
- Float: the shelf is a webview window that `float.rs` builds on first use. `grape_platform::windows::show_shelf` shows, moves and resizes it with `SetWindowPos(HWND_TOPMOST, …, SWP_NOACTIVATE | SWP_SHOWWINDOW)`, and `hide_shelf` hides it, on the main thread. Never use Tauri's `show`, `hide` or `set_focus` on it.
  - `harden_shelf` uses the overlay's subclass (D11) with other bits: `WS_EX_TOOLWINDOW` on (no taskbar button, not in Alt-Tab), `WS_EX_APPWINDOW` and `WS_EX_NOACTIVATE` off, and nothing added to position changes. So the shelf is activatable: a click on it activates Grape. The drag out needs that, because only the foreground window can capture the mouse.
  - It refuses drops (`refuse_drops`, D13) at creation and before every show, so a drag out of it dropped back on it does nothing.
  - It can't be minimized or maximized (`minimizable(false)`, `maximizable(false)`). Once a click has made it the active window the window keys reach it, and a minimized tool window has no taskbar button to come back from. A shelf minimized anyway counts as closed, so the next Float places it again, and `show_shelf` first restores it with `ShowWindow(SW_SHOWNOACTIVATE)`. The snap leaves a minimized shelf alone.
  - A close request (Alt+F4 on the active shelf) is the header's ×: `float::on_window_event` calls `prevent_close`, then empties and hides the shelf off the main thread. The window is never destroyed with its items kept.
  - Double-click or Enter reveals an item (`explorer.exe /select`). An item that's gone reveals nothing, because Explorer would open some other folder instead: the page doesn't ask for a row marked Missing, and `float_reveal` checks again (the page's view can be stale), logs it and sends a fresh view.
  - **Drag out** (`grape_platform::windows::drag_out`): `SHDoDragDrop` with the shell's own drop source and generic drag image, offering copy or link only, never move. The data object is Grape's own `IDataObject` (`ShelfData`). It serves two formats itself, each `GetData` a fresh `HGLOBAL` copy, and neither can be replaced: `CF_HDROP` (the block Copy puts on the clipboard), which has no common-parent rule, so files from several folders work, and `Preferred DropEffect` = copy. It never serves or stores a shell ID list (`Shell IDList Array`, `Shell Object Offsets`): asked for one, it fails with `DV_E_FORMATETC`, whatever anyone tried to store, and a CI test checks that `QueryGetData` for `CFSTR_SHELLIDLIST` fails. Every other format a caller stores with `SetData` (the drag image, a target's `Performed DropEffect`, a drop description) is kept in an inner, empty `SHCreateDataObject`, and only formats stored that way are served back or listed, since that object makes up formats of its own even when empty. If looking at the items before the drag takes over a second (a share that stopped answering), no drag starts: the press that asked for it may be over.
  - It runs on the main thread, which owns the window and OLE. The `float_drag_out` command is async and queues it there, because a synchronous command would run the drag loop inside WebView2's IPC callback.
  - `float_drag_out` first leaves out the items that are gone, on its own thread, so a share that stopped answering never stalls the main thread while the button is held. With none left, no drag is queued and their rows turn Missing.
  - Then, before the drag, `drag_out` checks that the calling thread owns the window, that a mouse button is still down (otherwise nothing starts: the drag would drop at once wherever the pointer is), that the paths are absolute and still there, and that no other drag out is running. It reads the button once more after the paths have been looked at, just before the drag starts, in case it came up meanwhile.
  - The result says copied, linked or cancelled. Some targets don't report what they did, so it's only logged.
  - **Watchdog:** during the drag a thread reads the buttons every 100 ms. Once no button has been down for 3 s, it posts Esc (`WM_KEYDOWN` and `WM_KEYUP`, `VK_ESCAPE`) to the window holding the drag thread's mouse capture (`GetGUIThreadInfo`), once, and logs it. If no window holds the capture, it posts the same Esc to the drag thread itself (`PostThreadMessageW`) instead. That ends a drag that missed the release, which would otherwise hold up the whole app.
- Copy: `CF_HDROP` + `Preferred DropEffect = DROPEFFECT_COPY` on the clipboard, so pasting in Explorer copies. Grape writes both with `SetClipboardData` from a message-only window it makes for the call, because `SetClipboardData` fails when the clipboard has no owner. The data is plain memory, not delayed rendering, so it stays after the window is gone, and there's no OLE data object to keep alive or flush.
- Move: `IFileOpenDialog` with `FOS_PICKFOLDERS`, titled "Move {name} to…" or "Move {n} items to…", starting in the first item's folder. Then `IFileOperation::MoveItem` with `FOF_ALLOWUNDO | FOFX_ADDUNDORECORD`, so the shell shows its own progress and name-clash questions. Whether Ctrl+Z in Explorer undoes a move made by another process is still to check on the PC. Items already in the picked folder are skipped. If any item may be on another volume than the folder, a Yes/No box asks first, with No as the default. Two checks feed that, and either is enough: the volume path (`GetVolumePathNameW`), and the volume serial number of each folder once junctions, symlinks and DFS links are followed (`GetFileInformationByHandle`). A volume that can't be read counts as another one. Everything runs on a thread of its own in a single-threaded COM apartment.
  - What moved is read back from the disk: an item moved only if its old path is gone while its old folder can still be read. A progress sink (`IFileOperationProgressSink::PostMoveItem`) supplies hints: where each item went, which matters after a "keep both" rename, and whether anything inside a folder moved. A folder still in its old place after a merge, a skip inside it or a cancel, and an item whose old place can't be read any more, are reported as unsure. The toast then says "Some of it may not have moved" and shows the destination, never "moved" or "stayed put".
  - Clicking the toast runs `explorer.exe /select,"<path>"` with the quotes written out (`raw_arg`). `Command::arg` would quote the whole switch when the path has a space and leave a comma bare.
- Copy Path: `CF_UNICODETEXT`, paths joined with `\r\n`.
- Terminal: with the default "Windows Terminal" setting, `wt.exe -d "<dir>"` if `wt` starts; otherwise, or when the user chose PowerShell in Settings, `powershell.exe -NoExit -Command "Set-Location -LiteralPath '<dir>'"` in a new console. PowerShell reads `'`, `‘`, `’`, `‚` and `‛` as the same quote, so each one in the path is doubled. wt splits its command line at `;`, so a path with `;` goes straight to PowerShell. Files open their parent folder.

## Conversions that use Windows components
- **HEIC decode:** bundled libheif + libde265 (LGPL, as DLLs) so it works without the Store HEIF extension.
- **HEIC encode:** bundled `heif-enc` sidecar built with x265. That makes the sidecar GPL, which is fine because Grape is free and open source and it's a separate executable (`ENGINES.md` licensing rules).
- **H.264 encode:** the FFmpeg sidecar with libx264, allowed under the same rules. Prefer `h264_mf` / `h264_nvenc` / `h264_qsv` / `h264_amf` hardware encoders when present, with x264 as the fallback.

## Sidecars

FFmpeg and ffprobe are separate programs that Grape runs (golden rule 6). They aren't in the repo. A script fetches the builds pinned in `scripts/sidecars.lock` (D25; the pin itself is D22). Run it from the repo root:

```powershell
.\scripts\fetch-sidecars.ps1          # fetch, unless they're already current
.\scripts\fetch-sidecars.ps1 -Force   # fetch again anyway
```

- It works in Windows PowerShell 5.1 and PowerShell 7. If PowerShell says running scripts is disabled on this system, run `powershell -ExecutionPolicy Bypass -File .\scripts\fetch-sidecars.ps1` instead. That changes no setting.
- **What lands where.** `apps/desktop/src-tauri/binaries/`, which git ignores, gets:
  - `ffmpeg-x86_64-pc-windows-msvc.exe` and `ffprobe-x86_64-pc-windows-msvc.exe`, the target-triple names Tauri's `bundle.externalBin` looks for. Both come from one BtbN zip (194 MB).
  - `ffmpeg-LICENSE.txt`, the build's GPLv3 text from that zip, kept for the bundle step. The script never writes into `licenses/`, which is committed and written by hand (`licenses/README.md`).
  - `sidecars.stamp`, written last: the lock's SHA-256, the target, and each file's SHA-256.
- **The checks.** The zip downloads into a new temp folder over HTTPS (TLS 1.2 or later). Its size and SHA-256 must match the lock before anything is extracted. On a mismatch it's deleted and nothing changes. Only the files the lock names are extracted. Each program then runs with `-hide_banner -version`: the first line must name the lock's version, and the configuration line must not have `--enable-nonfree`. Only after every check passes do the files replace the ones in `binaries/`. The temp folder is always deleted.
- **A second run** reads the stamp. If the lock, the target and every file still match, it says the sidecars are already current and stops. If anything differs (a new lock, a changed or missing file), it fetches again.
- It doesn't change `PATH` or install anything. This script is the only download, at build time. Grape itself never downloads anything (golden rule 3).
- **CI** runs it in the Sidecars job: in Windows PowerShell 5.1, then again in PowerShell 7, which must find them current. `binaries/` is cached by the hashes of the lock and both fetch scripts, so a changed script runs in full. The job lists the files and their sizes, and builds nothing.
- **Not in the installer yet.** `apps/desktop/src-tauri/tauri.sidecars.conf.json` adds both to `bundle.externalBin` for a build that passes it (from `apps/desktop`): `pnpm tauri build --bundles nsis --config src-tauri/tauri.sidecars.conf.json`. It also bundles `licenses/ffmpeg/`, so a build or `tauri dev` that passes it fails until that folder exists. No build passes it yet. The installer carries FFmpeg only once the media engine uses it and `licenses/ffmpeg/` (the license texts, and `SOURCE.md` with the source link) and its `THIRD_PARTY_NOTICES.md` entry exist (D25).
- **In dev**, `pnpm tauri dev --config src-tauri/tauri.sidecars.conf.json` makes tauri-build copy them into `target\debug` as `ffmpeg.exe` and `ffprobe.exe`, next to Grape's executable. Without the overlay they aren't copied there.
- **Installed**, they lose the suffix: the installer puts `ffmpeg.exe` and `ffprobe.exe` next to `Grape.exe` in `%LOCALAPPDATA%\Grape`. That's where `src/sidecars.rs` looks, never on `PATH`.
- `GRAPE_SIDECARS_NO_RUN=1` skips the `-version` runs and writes no stamp, so the download can be tested on another OS. It's for testing only. The next run without it checks everything again.

## Distribution
- Installer: Tauri **NSIS** bundle (per-user install, no admin needed) plus a portable `.zip`. Keep MSI for later.
- Code signing: unsigned builds trigger SmartScreen ("Windows protected your PC"). Free option for open source: **SignPath Foundation** (free signing for OSS projects that meet its criteria). Alternatives: Azure Trusted Signing (paid, identity checks). Log in DECISIONS when chosen.
- Launch at login: `tauri-plugin-autostart` (HKCU Run key).
- Builds: GitHub Actions `windows-latest` (x64) and `windows-11-arm` (ARM64) runners. See ROADMAP M7.

## Releasing

A version tag makes a draft release. `.github/workflows/release.yml` builds the installer from the tag and drafts a GitHub Release with it. It never publishes: Maxwell checks the draft and publishes it by hand. So far it builds x64 only.

**Cutting a release**

1. Pick the version: plain `X.Y.Z`, with no `-beta` or other suffix (the workflow refuses those tags). Set it in all four places:
   - `apps/desktop/src-tauri/tauri.conf.json` → `"version"` (the installer's version)
   - `Cargo.toml` → `[workspace.package]` → `version` (every crate inherits it)
   - `apps/desktop/package.json` → `"version"`
   - `apps/desktop/ui/package.json` → `"version"`
2. Run `cargo update --workspace`. It moves Grape's own four crates to the new version in `Cargo.lock` and leaves every other crate alone.
3. Commit the five files (`git commit -am "Release X.Y.Z"`) and push. Wait for CI to pass on that commit.
4. Tag the commit and push that one tag:

   ```powershell
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

   Push it by name, not with `git push --tags`. GitHub starts no workflow when more than three tags arrive in one push.

5. Open Actions → Release → the run for the tag, and wait for both jobs to finish. CI runs on the tag too.
6. Check the draft (Releases → "Grape X.Y.Z", marked Draft), following `TESTING.md` → "Release, M7":
   - Download `Grape-Setup-x64.exe` and `SHA256SUMS.txt` from the draft, and check the hash with the PowerShell line in the notes.
   - Install it on a clean standard user account (no admin rights), go through SmartScreen, and convert a file.
7. Edit the draft: replace the TODO under "What's new".
8. Publish it.

If the version check fails, nothing was built or released. Delete the tag (`git tag -d vX.Y.Z`, then `git push --delete origin vX.Y.Z`), fix the versions, commit and tag again. To run a tag again after a runner or network failure, use Actions → Release → Run workflow and type the tag. That button shows only once `release.yml` is on the repo's default branch. Before that, open the failed run and click **Re-run failed jobs**. A fix in the code needs a new version, because a tag stays where it is.

**What the workflow checks**
- The tag is `v` and then `X.Y.Z`, and `X.Y.Z` is the version in the four files and in `Cargo.lock`'s entries for Grape's own crates. It prints each one and names the ones that differ, before anything builds.
- The build is the CI job's (`ci.yml` → `build-windows`): the same actions, Node 22, Rust 1.99, `pnpm install --frozen-lockfile` and `pnpm tauri build --bundles nsis --ci`.
- The NSIS bundler made exactly one `*-setup.exe`. It's copied to `Grape-Setup-x64.exe`, the same name in every release, and hashed into `SHA256SUMS.txt` as `<hash>  Grape-Setup-x64.exe`, in lower-case hex (the `sha256sum` format). Both files are also the run's `Grape-Setup-x64` artifact.
- The release job checks the downloaded installer against `SHA256SUMS.txt`, fills in `{{VERSION}}` in `.github/release-notes.md`, and makes the draft with `gh release create --draft --verify-tag`.
- **Running it again.** If the draft exists, the run replaces its two files (`--clobber`) and keeps its notes, so "What's new" survives. If the release is already published, the run fails and changes nothing, so a published file always matches its hash.
- **Permissions.** The workflow's token is read-only. Only the release job can write (`contents: write`), and it runs no build code.
- It doesn't sign anything or add an updater, and Grape itself still makes no network calls (golden rule 3).

**Signing.** The installer and Grape are unsigned, so SmartScreen warns: "Windows protected your PC" → **More info** → **Run anyway**. The release notes say so. Signing will come from the SignPath Foundation, which is free for open-source projects (ROADMAP M7). Applying is Maxwell's step. Once it's approved, the workflow gets a signing step after the build and before the hash, so `SHA256SUMS.txt` covers the signed file, and the choice gets a DECISIONS entry.

**WebView2.** Grape's windows need Microsoft's WebView2 runtime. `tauri.conf.json` doesn't set `bundle.windows.webviewInstallMode`, so Tauri 2.12's default applies: `downloadBootstrapper`, silent (`WebviewInstallMode::default` in tauri-utils 2.10.1).
- The installer looks for WebView2 in the registry first. If it's there, nothing is downloaded.
- If it's missing, the installer downloads Microsoft's bootstrapper from `go.microsoft.com`, runs it silently, and then installs Grape. If the download fails, the install stops with an error.
- That download is the installer's, not Grape's. Windows 11 and current Windows 10 already have WebView2, so it rarely happens.
- A PC that is offline *and* has no WebView2 can't install Grape. The `offlineInstaller` mode would fix that, for about 127 MB more per download.

**Where it installs.** `%LOCALAPPDATA%\Grape`, for the current user only (`installMode: currentUser`). The folder holds Grape itself, `THIRD_PARTY_NOTICES.md` and `LICENSE.txt` (D23). The uninstaller removes that folder and the launch-at-login entry (the `Grape` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`). It keeps `%APPDATA%\io.github.sussyswimmer.grape\` (settings) and `%LOCALAPPDATA%\io.github.sussyswimmer.grape\` (logs and WebView2's data), unless the user ticks "Delete the application data".

## Windows QA must-checks
- Drag from Explorer, the Desktop, Downloads, search results, OneDrive placeholders (cloud-only files: Grape must hydrate or skip them, not hang), and zip folders opened in Explorer (virtual paths, which v1 should skip).
- Shift-drop never deletes the source.
- A drag out of the float shelf onto the Recycle Bin, plain or with Shift, never deletes the file.
- Explorer running elevated vs not.
- Two monitors at different DPI.
- Tablet/touch: Windows touch drags don't use the mouse hook path. Unsupported in v1, so make sure nothing breaks.
