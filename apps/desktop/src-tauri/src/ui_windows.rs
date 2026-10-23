//! Grape's webview windows: the hidden overlay and toast (declared in
//! tauri.conf.json) and the settings window (created on first use).

use std::sync::{Mutex, PoisonError};

use anyhow::Context as _;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// Label of the overlay window in tauri.conf.json.
pub const OVERLAY: &str = "overlay";
/// Label of the settings window.
pub const SETTINGS: &str = "settings";

/// WebView2 flags for every Grape window. They must be identical across
/// windows that share a WebView2 profile, so tauri.conf.json repeats this
/// exact string (a test checks). Setting any flags replaces wry's defaults,
/// so its `--disable-features=…` is kept, and autoplay is allowed so the
/// overlay's tick can play without a click (`docs/design/m1-drag-wheel.md`
/// §0.6). Ignored off Windows.
pub const BROWSER_ARGS: &str =
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required";

/// Held while Settings is looked up and, if needed, built. Tauri rejects a
/// duplicate label only once the first window is registered, which happens
/// after its native window exists. Without this, two quick requests (a
/// double-click on the shortcut while Grape runs, or a second launch plus a
/// tray click) could both build a Settings window.
static SETTINGS_LOCK: Mutex<()> = Mutex::new(());

/// Gets the overlay ready at startup. On Windows this hardens the window
/// (tool window, topmost, not activated when shown or moved) and logs its
/// extended style. On macOS it lifts the window to the pop-up menu level,
/// over the menu bar, the Dock and full-screen apps, on every Space. Never
/// call `set_focus`, `set_always_on_top` or `set_fullscreen` on the overlay:
/// the first activates Grape, and on macOS the others reset the level.
///
/// Must run on the main thread (Tauri's `setup`), which owns the window.
///
/// # Errors
/// Fails if the overlay window from tauri.conf.json doesn't exist.
pub fn prepare_overlay(app: &AppHandle) -> anyhow::Result<()> {
    let overlay = app
        .get_webview_window(OVERLAY)
        .context("the overlay window from tauri.conf.json is missing")?;

    #[cfg(target_os = "windows")]
    windows_overlay::harden(&overlay);
    #[cfg(target_os = "macos")]
    mac_overlay::harden(&overlay);
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = overlay;

    Ok(())
}

/// Gets the toast ready at startup: the overlay's tool-window, no-activate
/// styles, and a drop target that refuses everything. Main thread only.
///
/// # Errors
/// Fails if the toast window from tauri.conf.json doesn't exist.
pub fn prepare_toast(app: &AppHandle) -> anyhow::Result<()> {
    let toast = app
        .get_webview_window(crate::toast::TOAST)
        .context("the toast window from tauri.conf.json is missing")?;
    #[cfg(target_os = "windows")]
    windows_overlay::harden(&toast);
    #[cfg(target_os = "macos")]
    mac_overlay::harden(&toast);
    refuse_drops(app, &toast);
    Ok(())
}

/// Opens Settings, or brings it to the front if it's already open.
///
/// The window is built off the calling thread: creating a WebView2 window
/// from inside a menu or IPC handler can deadlock on Windows.
pub fn request_settings(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(err) = show_settings(&app) {
            log::error!("could not open Settings: {err}");
        }
    });
}

fn show_settings(app: &AppHandle) -> tauri::Result<()> {
    // The lock guards no data, so a panic in an earlier holder leaves
    // nothing broken: keep using it.
    let _creating = SETTINGS_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(window) = app.get_webview_window(SETTINGS) {
        refuse_drops(app, &window);
        window.unminimize()?;
        window.show()?;
        window.set_focus()?;
        return Ok(());
    }
    let window = WebviewWindowBuilder::new(app, SETTINGS, WebviewUrl::App("settings.html".into()))
        .title("Grape Settings")
        .inner_size(560.0, 420.0)
        .min_inner_size(420.0, 320.0)
        .center()
        .additional_browser_args(BROWSER_ARGS)
        .build()?;
    refuse_drops(app, &window);
    window.set_focus()?;
    Ok(())
}

/// Settings and the toast take no drops. On Windows, wry's and tao's drop
/// handlers would accept a file there without setting the drop effect,
/// which Explorer can read as a move (DECISIONS D13), so a refusing target
/// replaces them. Runs on the main thread, which owns the window; failures
/// are logged.
///
/// On macOS nothing is replaced: wry answers every file drag with Copy and
/// never touches the file (DECISIONS D18), so a file dropped on Settings or
/// the toast is ignored, never moved.
fn refuse_drops(app: &AppHandle, window: &tauri::WebviewWindow) {
    #[cfg(target_os = "windows")]
    {
        let window = window.clone();
        let queued = app.run_on_main_thread(move || {
            let hwnd = match window.hwnd() {
                Ok(hwnd) => hwnd.0 as isize,
                Err(err) => {
                    log::error!("{} drop guard: no window handle: {err}", window.label());
                    return;
                }
            };
            match grape_platform::windows::refuse_drops(hwnd) {
                Ok(coverage) => log::info!(
                    "{} refuses drops on {} of {} window(s)",
                    window.label(),
                    coverage.registered,
                    coverage.windows
                ),
                Err(err) => log::error!("{} drop guard: {err}", window.label()),
            }
        });
        if let Err(err) = queued {
            log::error!("drop guard: could not reach the main thread: {err}");
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = (app, window);
}

/// Debug builds only: shows the overlay for about two seconds, so focus,
/// taskbar and Alt-Tab behavior can be checked by hand. Logs the extended
/// style before, while and after it's shown.
#[cfg(debug_assertions)]
pub fn flash_overlay(app: &AppHandle) {
    use std::time::Duration;

    const FLASH: Duration = Duration::from_secs(2);

    let Some(overlay) = app.get_webview_window(OVERLAY) else {
        log::error!("the overlay window is missing");
        return;
    };
    #[cfg(target_os = "windows")]
    windows_overlay::log_ex_style(&overlay, "before flash");
    if let Err(err) = overlay.center().and_then(|()| overlay.show()) {
        log::error!("could not show the overlay: {err}");
        return;
    }
    #[cfg(target_os = "windows")]
    windows_overlay::log_ex_style(&overlay, "shown");

    std::thread::spawn(move || {
        std::thread::sleep(FLASH);
        if let Err(err) = overlay.hide() {
            log::error!("could not hide the overlay: {err}");
        }
        #[cfg(target_os = "windows")]
        windows_overlay::log_ex_style(&overlay, "hidden again");
    });
}

#[cfg(target_os = "windows")]
mod windows_overlay {
    use tauri::WebviewWindow;

    fn raw_hwnd(overlay: &WebviewWindow) -> Option<isize> {
        match overlay.hwnd() {
            Ok(hwnd) => Some(hwnd.0 as isize),
            Err(err) => {
                log::error!("could not get the overlay's window handle: {err}");
                None
            }
        }
    }

    /// Installs the style subclass (feature `overlay-subclass`, on by
    /// default) and logs the resulting extended style. Failures are logged,
    /// not fatal: Grape still runs, the overlay may just show in Alt-Tab.
    pub fn harden(overlay: &WebviewWindow) {
        let Some(hwnd) = raw_hwnd(overlay) else {
            return;
        };

        #[cfg(feature = "overlay-subclass")]
        match grape_platform::windows::harden_overlay(hwnd) {
            Ok(()) => log::info!("overlay: style subclass installed"),
            Err(err) => log::error!("overlay: could not install the style subclass: {err}"),
        }
        #[cfg(not(feature = "overlay-subclass"))]
        log::warn!("overlay: the overlay-subclass feature is off; tao's styles only");

        log_hwnd_ex_style(hwnd, "at startup");
    }

    /// Logs the overlay's current extended style, e.g.
    /// `overlay ex-style (shown): 0x08080088 TOOLWINDOW|NOACTIVATE|TOPMOST|LAYERED`.
    #[cfg(debug_assertions)]
    pub fn log_ex_style(overlay: &WebviewWindow, when: &str) {
        if let Some(hwnd) = raw_hwnd(overlay) {
            log_hwnd_ex_style(hwnd, when);
        }
    }

    fn log_hwnd_ex_style(hwnd: isize, when: &str) {
        match grape_platform::windows::ex_style(hwnd) {
            Ok(style) => log::info!(
                "overlay ex-style ({when}): {}",
                grape_platform::windows::describe_ex_style(style)
            ),
            Err(err) => log::warn!("overlay ex-style ({when}): unreadable: {err}"),
        }
    }
}

/// The macOS side of [`prepare_overlay`] and [`prepare_toast`].
#[cfg(target_os = "macos")]
mod mac_overlay {
    /// Lifts `window` over the menu bar, the Dock and full-screen apps, on
    /// every Space, and logs how it ended up. Main thread only.
    pub(super) fn harden(window: &tauri::WebviewWindow) {
        let ns_window = match window.ns_window() {
            Ok(ns_window) => ns_window,
            Err(err) => {
                log::error!("{}: no NSWindow: {err}", window.label());
                return;
            }
        };
        // SAFETY: `ns_window` came from Tauri on this, the main thread
        // (setup), just now, so it's a live NSWindow for this call.
        match unsafe { grape_platform::macos::harden_window(ns_window) } {
            // SAFETY: as above.
            Ok(()) => log::info!("{}: {}", window.label(), unsafe {
                grape_platform::macos::describe_window(ns_window)
            }),
            Err(err) => log::error!("could not harden the {} window: {err}", window.label()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn config_windows() -> Vec<Value> {
        let config: Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        config["app"]["windows"].as_array().cloned().unwrap()
    }

    /// WebView2 refuses to start a second window with other flags in the
    /// same profile, so every window must use exactly BROWSER_ARGS.
    #[test]
    fn every_configured_window_uses_the_shared_browser_args() {
        let windows = config_windows();
        assert!(!windows.is_empty());
        for window in windows {
            assert_eq!(
                window["additionalBrowserArgs"].as_str(),
                Some(BROWSER_ARGS),
                "{}",
                window["label"]
            );
        }
    }

    #[test]
    fn the_toast_is_hidden_topmost_and_never_focused() {
        let windows = config_windows();
        let toast = windows
            .iter()
            .find(|w| w["label"] == crate::toast::TOAST)
            .unwrap();
        assert_eq!(toast["url"], "toast.html");
        assert_eq!(
            (toast["width"].as_f64(), toast["height"].as_f64()),
            (Some(320.0), Some(64.0))
        );
        for (key, value) in [
            ("visible", false),
            ("transparent", true),
            ("decorations", false),
            ("alwaysOnTop", true),
            ("skipTaskbar", true),
            ("focus", false),
            ("focusable", false),
        ] {
            assert_eq!(toast[key].as_bool(), Some(value), "{key}");
        }
    }
}
