//! The tray icon and its menu: Settings…, Pause Grape, Quit.

use std::sync::atomic::Ordering;

use tauri::menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuEvent, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Wry};

use crate::{ui_windows, GrapeState};

type PauseItem = CheckMenuItem<Wry>;

const TRAY_ID: &str = "grape";

const ITEM_SETTINGS: &str = "settings";
const ITEM_PAUSE: &str = "pause";
const ITEM_QUIT: &str = "quit";
#[cfg(debug_assertions)]
const ITEM_FLASH_OVERLAY: &str = "debug-flash-overlay";

/// Creates the tray icon. Tauri keeps it alive for the app's lifetime.
pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let settings = MenuItemBuilder::with_id(ITEM_SETTINGS, "Settings…").build(app)?;
    let pause = CheckMenuItemBuilder::with_id(ITEM_PAUSE, "Pause Grape")
        .checked(false)
        .build(app)?;
    let quit = MenuItemBuilder::with_id(ITEM_QUIT, "Quit").build(app)?;

    let menu = MenuBuilder::new(app).item(&settings).item(&pause);
    // Debug builds only: lets the overlay's focus, taskbar and Alt-Tab
    // behavior be checked by hand before M1 shows it for real.
    #[cfg(debug_assertions)]
    let menu = menu
        .separator()
        .text(ITEM_FLASH_OVERLAY, "Debug: flash overlay");
    let menu = menu.separator().item(&quit).build()?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(tauri::include_image!("icons/32x32.png"))
        .tooltip(tooltip(false))
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| on_menu_event(app, &event, &pause))
        .build(app)?;
    Ok(())
}

fn on_menu_event(app: &AppHandle, event: &MenuEvent, pause: &PauseItem) {
    match event.id().as_ref() {
        ITEM_SETTINGS => ui_windows::request_settings(app),
        ITEM_PAUSE => toggle_pause(app, pause),
        ITEM_QUIT => {
            log::info!("quit from the tray");
            app.exit(0);
        }
        #[cfg(debug_assertions)]
        ITEM_FLASH_OVERLAY => ui_windows::flash_overlay(app),
        other => log::warn!("unknown tray menu item {other:?}"),
    }
}

/// Flips the paused state. The state is the source of truth; the check mark
/// and tooltip are set to match it.
fn toggle_pause(app: &AppHandle, pause: &PauseItem) {
    let state = app.state::<GrapeState>();
    let paused = !state.paused.load(Ordering::Acquire);
    state.paused.store(paused, Ordering::Release);
    log::info!("{}", if paused { "paused" } else { "resumed" });

    if let Err(err) = pause.set_checked(paused) {
        log::warn!("could not update the Pause check mark: {err}");
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Err(err) = tray.set_tooltip(Some(tooltip(paused))) {
            log::warn!("could not update the tray tooltip: {err}");
        }
    }
}

fn tooltip(paused: bool) -> &'static str {
    if paused {
        "Grape (paused)"
    } else {
        "Grape"
    }
}
