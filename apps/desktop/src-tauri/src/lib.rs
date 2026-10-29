//! Grape's Tauri app: a tray icon, a hidden overlay window, the wheel
//! controller, the tool panels, the Settings window's settings and, on
//! Windows and macOS, the float shelf. See `CLAUDE.md` in this crate.
//!
//! There is no main window. On Windows the app lives in the notification
//! area; on macOS it's a menu-bar accessory app with no Dock icon
//! (DECISIONS D18).

mod actions;
mod contain;
pub mod controller;
pub mod events;
mod float;
mod jobs;
mod panel;
mod settings;
mod sidecars;
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
        // starts: it opens Settings in the running instance and exits. A
        // second launch from launch at login (Grape already runs) opens
        // nothing, as a first one doesn't.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if settings::at_login(&args) {
                log::info!("second launch at login: Grape already runs; nothing to open");
                return;
            }
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
        // One cancel token per running drop, so quitting can stop them.
        .manage(jobs::Running::default())
        // Empty, and only ever filled on Windows and macOS; the commands
        // work on it everywhere.
        .manage(float::Shelf::default())
        // At most one tool panel, and what it waits on.
        .manage(panel::PanelState::default())
        .invoke_handler(tauri::generate_handler![
            wheel::wheel_painted,
            toast::toast_clicked,
            float::float_view,
            float::float_remove,
            float::float_clear,
            float::float_reveal,
            float::float_drag_out,
            panel::panel_current,
            panel::panel_run,
            panel::panel_cancel,
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
        .on_window_event(float::on_window_event)
        .on_window_event(panel::on_window_event)
        .setup(|app| {
            // A menu-bar app on macOS: no Dock icon and no menu bar shown.
            // Tauri's default menu still exists: its Close Window (Cmd+W)
            // is the float shelf's close request.
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
            // Not fatal: tools then run without their panels' choices.
            if let Err(err) = ui_windows::prepare_panel(app.handle()) {
                log::error!("could not prepare the panel window: {err:#}");
            }
            // Hidden, now: building a webview activates Grape, which the
            // first Float mustn't (D43). Not fatal: that Float tries again.
            #[cfg(target_os = "macos")]
            float::build_at_launch(app.handle());
            panel::set_preview(app.handle(), panel::preview_from_env());
            // Before any drop runs: the engines put each helper process
            // they start into a Job Object on Windows (D33).
            contain::install();
            // What a crash or a kill left in the temp folder (D33).
            contain::remove_leftovers();
            // Once, next to Grape's executable; every drop gets a copy.
            app.manage(sidecars::Sidecars::find());
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
        // Grape is going (tray → Quit, or a restart): stop every job now,
        // and forget the open panel so a late Run is ignored.
        RunEvent::ExitRequested { code: Some(_), .. } => {
            panel::shut(app);
            jobs::cancel_all(app);
        }
        // Unhook the mouse before the process goes, and give the cancelled
        // jobs a moment to kill their children and delete their temp
        // files, so none outlives Grape. Cancelling again covers an exit
        // that skipped the request.
        RunEvent::Exit => {
            panel::shut(app);
            jobs::cancel_all(app);
            wheel::stop(app);
            jobs::wait_for_all(app);
        }
        // Opening Grape again while it runs (Finder, Launchpad, Spotlight)
        // opens Settings, like a second launch does on Windows.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => ui_windows::request_settings(app),
        _ => {}
    });
    Ok(())
}
