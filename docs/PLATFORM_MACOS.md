# macOS platform notes

Target: macOS 13+ on **Apple silicon and Intel**. Ship a universal binary, since Tangerine is Apple-silicon-only and Intel support is cheap for us. Rust bindings: `objc2`, `objc2-app-kit`, `objc2-foundation`.

## Detecting a file drag without special permissions

Goal: no Accessibility or Input Monitoring prompt. **[Grape design]** It's *inferred*, not observed, that Tangerine works similarly, since it ships sandboxed on the Mac App Store.

Built as `grape_platform::macos::MacDragMonitor` (D18):

1. **Drag pasteboard.** `NSPasteboard(name: .drag)` (`NSPasteboardNameDrag`) changes its `changeCount` when a new drag session begins anywhere in the system.
2. **Poll only while the mouse is down.** A `grape-drag` thread checks `NSEvent.pressedMouseButtons() & 1` every 100 ms while the button is up, and every 8 ms (~120 Hz) while the left button is held. (A global `NSEvent` mouse monitor was the alternative; polling needs no run loop and was simpler.)
3. **Started:** as soon as a poll sees that the drag pasteboard's `changeCount` changed since the button was last up and its `types` include `public.file-url`, the tracker sends `DragSignal::Started`. AppKit only starts a drag session once the pointer has moved past its own threshold, so Grape adds no 4-point check of its own. The keys held when Grape first sees that drag session are the baseline and don't open a wheel (D14), as on Windows where they're read at the threshold. A key pressed after that opens the wheel, even while the pointer rests. A key tapped and released before Grape sees the session (up to a tenth of a second) is missed. Known gap: if a whole file drag ends and a new press starts within one idle poll, the new press looks like a file drag too; a key tapped during a plain click-and-hold then shows an empty ring, which closes on the next move or on release. Grape checks only the *types*. It never reads the pasteboard's contents, because recent macOS can ask the user before an app reads a pasteboard. The paths arrive with the overlay's drop `Enter`, as on Windows.
4. **Modifiers:** poll `NSEvent.modifierFlags()` (class property, no permission needed) on the same thread. Map `.shift`, `.control`, `.option` and `.command` to shift, ctrl, alt and meta.
5. **Cursor:** `NSEvent.mouseLocation()` is in *bottom-left-origin* points. Grape's global coordinates on macOS are **points with the origin at the top-left of the primary (menu-bar) screen**, y down, the same space as `CGDisplayBounds`. With H the primary screen's height: cursor = (x, H − y), and a screen frame F becomes {F.x, H − (F.y + F.h), F.w, F.h}. `DisplayBounds.scale` is 1.0, so 1 unit = 1 point = 1 CSS pixel in the overlay. Screens left of or above the primary one have negative coordinates.
6. **Drag ends** when the button goes up → `DragSignal::Ended`.
7. **Esc** can't be read without Input Monitoring (D7), so `Escape` is never sent. Esc cancels the AppKit drag session, which the overlay sees as a drop `Leave` while the button is still down; the controller treats that as Esc (below). That only works while a wheel is open and the cursor is over it on the wheel's display. Esc before any wheel opens can't be seen at all: a key tap while still holding the button then shows an empty ring, which closes on the next move or on release. Esc on another display leaves the ring up until the button goes up. The drag itself is cancelled either way, so nothing is dropped.
8. **App Nap:** with every window hidden, macOS may nap a menu-bar app and stretch its timers. The drag thread holds an `NSProcessInfo` activity (user-initiated, idle system sleep allowed) for as long as it runs, so its polls stay on time.

If the drag pasteboard has no file URL type (text, browser images, promised files), the wheel never opens.

**File promises** (e.g. dragging from Photos or Mail) arrive as `NSFilePromiseReceiver`. Treat them as v2 and close the wheel for now.

## Overlay window
- Tauri window as described in `ARCHITECTURE.md`. `macos::harden_window` (main thread) sets level `.popUpMenu` (101) and adds `.canJoinAllSpaces`, `.fullScreenAuxiliary`, `.stationary` and `.ignoresCycle` to `collectionBehavior`, with `hidesOnDeactivate` off. Never use a level of 500 or more: the system's drag image sits there, and covering it hides what the user is dragging. The toast gets the same behavior one level lower (100, `WindowRole::Toast`), so the full-display overlay always covers it and a progress update can't put it above an open wheel.
- **Showing:** `macos::frame_and_show` sets the frame (Grape's top-left points turned into a Cocoa rect) and calls `orderFrontRegardless` in one main-thread call; that orders the window front although Grape isn't the active app. `"focusable": false` is what keeps the overlay and toast from ever becoming key. Never call `activate`, `set_focus`, `set_always_on_top` or `set_fullscreen` on them: the first activates Grape, and tao's level setters reset the level. Hiding uses Tauri's `hide()`.
- `"backgroundThrottling": "disabled"` keeps a hidden overlay's page running (macOS 14+), so the wheel is drawn by the time the window appears. The toast has `"acceptFirstMouse": true`, so one click on it reveals the output while another app is active.
- If drops over full-screen apps fail, switch to a non-activating `NSPanel` (`tauri-nspanel`). Logged as a known risk; check it with the `mac:` QA lines.
- The overlay must not activate Grape, so the frontmost app keeps focus. The whole app uses `ActivationPolicy::Accessory` (menu-bar only, no Dock icon), set before the event loop starts, with `LSUIElement` in `Info.plist`. Opening Grape.app again opens Settings.

## Drops on the overlay
- wry's own drop handler is the drop target, reported as Tauri's `WindowEvent::DragDrop` (`dragDropEnabled: true`). It accepts the drag as a **copy**, so Finder never moves the file. Its positions are points relative to the window's top-left (typed as `PhysicalPosition`), and the app adds the overlay's frame origin to make global `DropEvent`s.
- **Command or Control held:** Finder limits the drag to "generic" or "link" while those keys are down, and AppKit may then refuse wry's copy, so no `Drop` arrives. With Command or Control held at release over the overlay and no `Drop` within the drop grace (750 ms), the controller runs the highlighted segment itself (`ControllerConfig::release_fallback`). A refused drop's `Leave` can come just before or just after the button-up, so a release within 50 ms after a `Leave` counts too; a later release (Esc, then letting go) runs nothing. It never runs twice. Shift drops are normal drops.
- **Esc:** a `Leave` while the button is still down, with the cursor still on the wheel's display, starts a 200 ms grace (`ControllerConfig::leave_ends_drag`). An `Enter` or `Over` within it was a flicker, and the wheel carries on. A button-up within 50 ms of the `Leave` is a release (above). A move onto another display keeps the wheel with nothing highlighted. Otherwise the wheel closes as for Esc and stays out of the way until the button goes up.
- **The toast** has `dragDropEnabled` too, so wry accepts a file dropped on it as a copy and nothing happens; the file is never moved. While a wheel is open, the overlay covers it.

## Haptics and sound
- Tick: `NSHapticFeedbackManager.defaultPerformer.perform(.alignment, performanceTime: .now)`, as `grape_platform::haptic_tick()`, on each new highlighted segment while the "haptics" setting is on (D19).
- Sound: play our own short tick via `NSSound` (preloaded) or WebAudio in the overlay. It must start within 10 ms.

## File actions
- Share: `NSSharingService(named: .sendViaAirDrop)?.perform(withItems: urls)` on the main thread, after activating Grape so the AirDrop window comes to the front. A delegate hands activation back to the app that was in front once AirDrop reports it's done, failed or was cancelled, unless an AirDrop window is still open; then Grape stays active until the user clicks elsewhere.
- Copy: write `NSURL`s to `NSPasteboard.general` after `clearContents()`. Copy Path: the paths as one string, one per line.
- Move: `NSOpenPanel` ("Move Here", directories only) on the main thread, then `FileManager.moveItem` on the action's own thread. It never overwrites: an item whose name is taken in the chosen folder stays where it was. A mounted volume's root (a USB stick, a DMG or a network share dragged from the Desktop or the sidebar) is never moved, because moving it would copy its contents and then erase them. There's no system undo for this, so moving to another volume asks "Move to another drive?" first, with Cancel as the default button. Grape activates itself for the panel and the alert, then hands activation back to the app that was in front. Each item's result is recorded right after its own move, and the toast says what moved and reveals it.
- Terminal: `open -a Terminal <dir>`, or `open -b com.googlecode.iterm2 <dir>` when the user chose iTerm2 in Settings and Launch Services finds it (otherwise Terminal, with a log line).
- Reveal (toast click): `open -R <path>`.
- Float: not on the Mac yet. Control-drop on FLOAT shows "Not in this build yet", because a drag out of the shelf needs an `NSDraggingSession` (D21).

## Conversions that use Apple frameworks
- **HEIC encode/decode:** ImageIO (`CGImageDestination` with `public.heic`). It's the native, licence-free path, so prefer it over libheif on macOS.
- **H.264/HEVC encode:** FFmpeg `h264_videotoolbox` / `hevc_videotoolbox`. Hardware, fast, and avoids x264 on Mac.
- **v2 documents:** `NSAttributedString` reads and writes `docx`, `rtf`, `rtfd`, `html` and `odt` natively. **[inferred]** This is a plausible route for Tangerine's DOCX/RTF/RTFD support. Text clippings are `.textClipping` resource-fork-style plists; read the `utf8` / `public.utf8-plain-text` data.
- **Pages:** no public API. `NSAttributedString` can't read `.pages`. See `DECISIONS.md` O2.

## Running a CI build

Every CI run builds `Grape-macOS-universal`, a DMG with a universal (Apple silicon + Intel) Grape.app. It's ad-hoc signed but not notarized (D18; notarization comes with M9), so macOS will refuse to open it at first.

1. Open the run on GitHub (Actions → CI → the run for the commit). Under **Artifacts**, download `Grape-macOS-universal` and unzip it.
2. Open the `.dmg` and drag **Grape** into **Applications**.
3. Launch Grape once. macOS says it can't check it for malware. Click **Done**.
4. Open **System Settings → Privacy & Security**, scroll to Security, and click **Open Anyway** next to "Grape was blocked". Confirm with your password.
   - Or, in Terminal: `xattr -dr com.apple.quarantine /Applications/Grape.app`, then open Grape normally.
5. Grape appears in the menu bar (no Dock icon). Quit it from its menu.

## Distribution
- **Direct download first:** a DMG signed with **Developer ID**, hardened runtime, **notarized** and stapled. Every sidecar binary must be signed with the same identity and hardened runtime, or notarization fails.
- Entitlements: none needed for drag/modifier polling. Add `com.apple.security.cs.disable-library-validation` **only** if a sidecar loads unsigned dylibs (avoid this).
- Mac App Store is **later**. The sandbox complicates writing outputs next to sources: a dropped file grants access to that file, not its folder. It would need security-scoped bookmarks or a user-chosen output folder. Tracked in `DECISIONS.md`.
- Auto-update: `tauri-plugin-updater` with signed update manifests on GitHub Releases. It's the *only* allowed network call (rule 3), and it's opt-in.

## Mac QA must-checks
- Drag from Finder, the Desktop, Downloads stack, Spotlight results, and the Finder sidebar.
- Full-screen app in front. Two displays with different scale factors. Stage Manager on.
- Drop outside the ring cancels and Finder does **not** move the file.
- The full list is in `TESTING.md` → "macOS, first build (D18)".
