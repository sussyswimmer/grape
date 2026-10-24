//! Grape's Tauri app: a tray icon, a hidden overlay window, the wheel
//! controller and the Settings window's settings. See `CLAUDE.md` in this
//! crate.
//!
//! There is no main window. On Windows the app lives in the notification
//! area; on macOS it's a menu-bar accessory app with no Dock icon
//! (DECISIONS D18).

mod actions;
pub mod controller;
pub mod events;
mod jobs;
mod settings;
mod toast;
mod tray;
mod ui_windows;
mod wheel;

use std::sync::atomic::AtomicBool;

use anyhow::Context as _;
use tauri::{Manager, RunEvent};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_log::RotationStrategy;

use crate::settings::SettingsState;

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
    #[cfg_attr(
        not(target_os = "macos"),
        expect(unused_mut, reason = "only macOS changes the app before it runs")
    )]
    let mut app = tauri::Builder::default()
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
        // settings.json in the app data folder. No window may call the
        // store's own commands; Settings goes through `settings_*`.
        .plugin(tauri_plugin_store::Builder::new().build())
        // Launch at login (a Launch Agent on macOS). Driven from Rust only.
        // The argument lets a login launch skip the welcome card.
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![settings::LOGIN_ARG]),
        ))
        .manage(GrapeState::default())
        .manage(toast::Toaster::default())
        .invoke_handler(tauri::generate_handler![
            wheel::wheel_painted,
            toast::toast_clicked,
            settings::settings_get,
            settings::settings_set_key,
            settings::settings_set_output_next_to_source,
            settings::settings_pick_folder,
            settings::settings_use_default_fallback,
            settings::settings_set_toggle,
            settings::settings_set_terminal,
            settings::settings_dismiss_welcome
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
            // Before the wheel, which starts with the user's keys, sounds
            // and haptics.
            app.manage(SettingsState::load(app.handle()));
            let settings = app.state::<SettingsState>();
            wheel::start(
                app.handle(),
                settings.controller_config(),
                settings.haptics(),
            );
            // The welcome card, until it's dismissed: Settings opens the way
            // the tray's Settings… opens it, off this thread.
            if settings.welcome_at_launch(settings::launched_at_login()) {
                ui_windows::request_settings(app.handle());
            }
            log::info!("Grape {} is running", env!("CARGO_PKG_VERSION"));
            Ok(())
        })
        .build(tauri::generate_context!())
        .context("failed to build the Tauri app")?;

    // Before the loop starts, so Grape launches as an accessory app instead
    // of flashing a Dock icon first (tao applies this policy at launch).
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);

    app.run(|app, event| match event {
        // A tray app keeps running when its last window closes. Only an
        // explicit exit (tray → Quit calls `app.exit(0)`) carries a code.
        RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        // Unhook the mouse before the process goes.
        RunEvent::Exit => wheel::stop(app),
        // Opening Grape again while it runs (Finder, Launchpad, Spotlight)
        // opens Settings, like a second launch does on Windows.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => ui_windows::request_settings(app),
        _ => {}
    });
    Ok(())
}
