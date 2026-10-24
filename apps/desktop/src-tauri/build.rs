fn main() {
    // App commands are listed so each gets its own permission
    // (`allow-wheel-painted`, `allow-settings-get`, …), granted only to the
    // window that needs it in capabilities/. Unlisted commands can't be
    // called at all.
    let attributes =
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
            "wheel_painted",
            "toast_clicked",
            "settings_get",
            "settings_set_key",
            "settings_set_output_next_to_source",
            "settings_pick_folder",
            "settings_use_default_fallback",
            "settings_set_toggle",
        ]));
    if let Err(err) = tauri_build::try_build(attributes) {
        panic!("tauri-build failed: {err:#}");
    }
}
