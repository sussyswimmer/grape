# macOS platform notes

Target: macOS 13+ on **Apple silicon and Intel**. Ship a universal binary, since Tangerine is Apple-silicon-only and Intel support is cheap for us. Rust bindings: `objc2`, `objc2-app-kit`, `objc2-foundation`.

## Detecting a file drag without special permissions

Goal: no Accessibility or Input Monitoring prompt. **[Grape design]** It's *inferred*, not observed, that Tangerine works similarly, since it ships sandboxed on the Mac App Store.

1. **Drag pasteboard.** `NSPasteboard(name: .drag)` (`NSPasteboardNameDrag`) changes its `changeCount` when a new drag session begins anywhere in the system.
2. **Poll only while the mouse is down.** `NSEvent.pressedMouseButtons() & 1` is cheap. Run a ~120 Hz timer only while the left button is held; idle cost is a 10 Hz check of the button state.
   - Alternative: a global `NSEvent` monitor for `.leftMouseDown/.leftMouseDragged/.leftMouseUp`. Mouse monitors don't need Accessibility (key monitors do). Spike both in M1 and keep the cheaper one.
3. **When `changeCount` increments while the button is down and the pointer moved > 4 px** → `DragSignal::Started`. Read `readObjects(forClasses: [NSURL], options: [.urlReadingFileURLsOnly: true])` from the drag pasteboard to get file URLs **before** showing the overlay, so the wheel renders instantly.
4. **Modifiers:** poll `NSEvent.modifierFlags()` (class property, no permission needed) on the same timer. Map `.shift`, `.command`, `.control`, `.option`.
5. **Cursor:** `NSEvent.mouseLocation()` is in *bottom-left-origin* global coordinates. Convert to top-left-origin using the primary screen's height before sending to the app. Multi-monitor setups have negative coordinates; test that.
6. **Drag ends** when the button goes up → `DragSignal::Ended`.

If the drag pasteboard holds no file URLs (text, browser images, promised files), don't open the wheel.

**File promises** (e.g. dragging from Photos or Mail) arrive as `NSFilePromiseReceiver`. Treat them as v2 and close the wheel for now.

## Overlay window
- Tauri window as described in `ARCHITECTURE.md`. Set `NSWindow.collectionBehavior` to include `.canJoinAllSpaces`, `.fullScreenAuxiliary` and `.stationary`, with level `.popUpMenu` or higher.
- If drops over full-screen apps fail, switch to a non-activating `NSPanel` (`tauri-nspanel`). Logged as a known M1 risk.
- The overlay must not activate Grape, so the frontmost app keeps focus. Use `ActivationPolicy::Accessory` for the whole app (menu-bar only, no Dock icon).
- Coordinates: Tauri positions are physical pixels, AppKit uses points. Convert with the monitor's scale factor.

## Haptics and sound
- Tick: `NSHapticFeedbackManager.defaultPerformer.perform(.alignment, performanceTime: .now)`.
- Sound: play our own short tick via `NSSound` (preloaded) or WebAudio in the overlay. It must start within 10 ms.

## File actions
- Share: `NSSharingService(named: .sendViaAirDrop)?.perform(withItems: urls)`.
- Copy: write `NSURL`s to `NSPasteboard.general` after `clearContents()`.
- Move: `NSOpenPanel` (canChooseDirectories, !canChooseFiles), then `FileManager.moveItem`. There's no system undo for this, so confirm before moving across volumes and show a "Moved to …" toast with Reveal.
- Terminal: `open -a Terminal <dir>`, or `open -a iTerm <dir>`.

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
