fn main() {
    // App commands are listed so each gets its own permission
    // (`allow-wheel-painted`), granted only to the windows that need it in
    // capabilities/. Unlisted commands can't be called at all.
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(&["wheel_painted"]));
    if let Err(err) = tauri_build::try_build(attributes) {
        panic!("tauri-build failed: {err:#}");
    }
}
