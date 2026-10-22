//! Grape's webview windows: the hidden overlay (declared in tauri.conf.json)
//! and the settings window (created on first use).

use anyhow::Context as _;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// Label of the overlay window in tauri.conf.json.
pub const OVERLAY: &str = "overlay";
/// Label of the settings window.
pub const SETTINGS: &str = "settings";

/// Gets the overlay ready at startup. On Windows this hardens its extended
/// style (tool window, no activation, topmost) and logs the result.
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
    #[cfg(not(target_os = "windows"))]
    let _ = overlay;

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
    if let Some(window) = app.get_webview_window(SETTINGS) {
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
        .build()?;
    window.set_focus()?;
    Ok(())
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
