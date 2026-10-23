//! Grape's Tauri app: a tray icon, a hidden overlay window and the wheel
//! controller. See `CLAUDE.md` in this crate.
//!
//! There is no main window. On Windows the app lives in the notification
//! area; on macOS it will be a menu-bar accessory app (M8).

mod actions;
pub mod controller;
pub mod events;
mod jobs;
mod toast;
mod tray;
mod ui_windows;
mod wheel;

use std::sync::atomic::AtomicBool;

use anyhow::Context as _;
use tauri::RunEvent;
use tauri_plugin_log::RotationStrategy;

/// App-wide state, shared by the tray handlers and the controller.
#[derive(Debug, Default)]
pub struct GrapeState {
    /// Set by tray → "Pause Grape". While paused, drags behave as if Grape
    /// weren't installed.
    pub paused: AtomicBool,
}

/// Builds and runs the app. Returns when the user quits.
///
/// # Errors
/// Fails if the app can't be built (bad config or a plugin that won't
/// start).
///
/// A failure inside setup never comes back here: Tauri runs setup on the
/// event loop and panics if it returns an error. So setup logs every
/// failure first, and only fails for one Grape can't run without (no tray
/// icon means no way to reach Settings or Quit). The others are logged and
/// Grape keeps running.
pub fn run() -> anyhow::Result<()> {
    let app = tauri::Builder::default()
        // Registered first so a second launch is caught before anything else
        // starts: it opens Settings in the running instance and exits.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            log::info!("second launch: opening Settings in the running instance");
            ui_windows::request_settings(app);
        }))
        // Logs go to stdout and to the app log folder, rotated. Paths and
        // sizes only, never file contents.
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .rotation_strategy(RotationStrategy::KeepSome(5))
                .max_file_size(1_000_000)
                .build(),
        )
        .manage(GrapeState::default())
        .manage(toast::Toaster::default())
        .invoke_handler(tauri::generate_handler![
            wheel::wheel_painted,
            toast::toast_clicked
        ])
        .on_window_event(wheel::on_overlay_window_event)
        .setup(|app| {
            // A menu-bar app on macOS: no Dock icon, no app menu.
            #[cfg(target_os = "macos")]
            app.handle()
                .set_activation_policy(tauri::ActivationPolicy::Accessory)?;

            tray::create(app.handle()).inspect_err(|err| {
                log::error!("could not create the tray icon: {err}");
            })?;
            // Not fatal: the tray still works, the wheel just won't show.
            if let Err(err) = ui_windows::prepare_overlay(app.handle()) {
                log::error!("could not prepare the overlay window: {err:#}");
            }
            if let Err(err) = ui_windows::prepare_toast(app.handle()) {
                log::error!("could not prepare the toast window: {err:#}");
            }
            wheel::start(app.handle());
            log::info!("Grape {} is running", env!("CARGO_PKG_VERSION"));
            Ok(())
        })
        .build(tauri::generate_context!())
        .context("failed to build the Tauri app")?;

    app.run(|app, event| match event {
        // A tray app keeps running when its last window closes. Only an
        // explicit exit (tray → Quit calls `app.exit(0)`) carries a code.
        RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        // Unhook the mouse before the process goes.
        RunEvent::Exit => wheel::stop(app),
        _ => {}
    });
    Ok(())
}
