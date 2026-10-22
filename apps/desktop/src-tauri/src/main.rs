// Release builds use the Windows GUI subsystem, so no console window opens
// next to the tray icon. Debug builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(err) = grape_app::run() {
        eprintln!("Grape could not start: {err:#}");
        std::process::exit(1);
    }
}
