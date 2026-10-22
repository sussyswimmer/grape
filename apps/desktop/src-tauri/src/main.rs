// Release builds use the Windows GUI subsystem, so no console window opens
// next to the tray icon. Debug builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(err) = grape_app::run() {
        // Release builds have no console, so stderr goes nowhere. The log
        // file catches it too, if the logger was already running.
        log::error!("Grape could not start: {err:#}");
        eprintln!("Grape could not start: {err:#}");
        std::process::exit(1);
    }
}
