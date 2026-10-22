# Decisions log

Append-only. Each entry records the decision, why it was made, and what would change it. Open questions live at the bottom until they're decided.

## D1: Tauri 2, single codebase (2026-10-19, decided by Maxwell)
- **Decision:** one Tauri 2 app for macOS and Windows. Shared Rust core and TS wheel UI, with thin per-OS modules.
- **Why:** one conversion engine and one wheel implementation, split cleanly between Claude Code and Codex lanes.
- **Revisit if:** the overlay can't float or receive drops reliably on one OS after the M1 spike. Fallback: a native overlay (AppKit `NSPanel` / Win32 layered window) hosted by the same Rust app.

## D2: Grape is free and open source (2026-10-19, decided by Maxwell; open-source part assumed — confirm)
- **Decision:** free, no paid tier, no conversion quota. Source public on GitHub. App code license: **MIT (assumed default, confirm)**.
- **Consequence for codecs:** GPL tools (FFmpeg with x264, x265/`heif-enc`, pandoc) may ship as **separate sidecar executables** with license texts and source links. Never link GPL code into Grape's own binaries. See `ENGINES.md`.
- **Revisit if:** Grape ever goes paid or closed-source. Then re-audit every sidecar and drop GPL builds for LGPL builds plus hardware encoders.

## D3: Clean-room rule
- **Decision:** Tangerine's public website demo was analyzed with REA for **behavior only**. No Tangerine code, assets, copy, sounds or branding go into Grape. The Tangerine app binary is **not** to be decompiled: its App Store license restricts reverse engineering, and Grape doesn't need it.
- **Why:** legal safety and originality. The behavior spec in `docs/` is enough.

## D4: Direct distribution before stores
- **Decision:** v1 ships as a notarized DMG (macOS) and NSIS installer (Windows) on GitHub Releases. Mac App Store and Microsoft Store are later.
- **Why:** the Mac sandbox complicates writing outputs next to sources. Store review is slow for a fast-moving v1.
- **Note:** Developer ID signing and notarization need an Apple Developer Program membership ($99/yr). The account holder must be of legal age, so a parent or guardian can hold it. Without it, users must approve the app in System Settings → Privacy & Security on first launch. The README should explain this if we ship unsigned.

## D5: Outputs next to the source, never overwrite
- **Decision:** default output location is the source's folder, with `" 2"` collision naming. Fall back to `~/Downloads/Grape` if the folder isn't writable.
- **Why:** observed in Tangerine's naming and matches user expectation.

## D6: Windows default keys Shift / Ctrl / Alt
- **Why:** the Windows key opens Start and can't be used mid-drag. Remappable.

## D7: No Accessibility/Input Monitoring permission on macOS
- **Decision:** detect drags via the drag pasteboard and poll modifiers and mouse state. Never request Accessibility.
- **Revisit if:** polling proves unreliable in M1. Then ask for the permission with a clear onboarding explanation.

## D8: Windows first, macOS second (2026-10-19, decided by Maxwell)
- **Decision:** Phase 1 builds and ships Grape for Windows. The macOS port (Phase 2, ROADMAP M8–M9) starts after Windows v1 ships.
- **How it's built:** Maxwell builds with Claude Code on his own Windows PC, which is also the test machine for every manual QA item.
- **Consequence:** until M8, `grape-platform` keeps a stub `macos` module that compiles but does nothing. Don't spend time on AppKit code in Phase 1. Shared code (`grape-core`, `grape-engines`, the UI) must stay OS-neutral so the port is cheap.

---

## Open questions

- **O1: Confirm the open-source license.** MIT is assumed. GPL-3.0 is the alternative if you want forks to stay open.
- **O2: Apple Pages support.** There's no open library and `NSAttributedString` can't read `.pages`. How Tangerine does it is unknown; the website doesn't reveal it. Options: (a) skip; (b) macOS-only via scripting the Pages app if installed; (c) parse the IWA format (large effort). Default: skip until users ask.
- ~~**O3: Windows test hardware.**~~ Resolved: Maxwell has his own Windows PC (see D8).
- **O4: Optional add-on downloads** (LibreOffice for slides/office in v2). This needs a network call outside the conversion path. Decide before v2.
- **O5: Show the wheel for drags from any app on Windows,** or only Explorer and the Desktop (the default)?
