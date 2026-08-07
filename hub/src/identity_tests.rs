use std::path::Path;

use tauri::utils::config::Config;

use super::{apply_to_config, load_icon, Identity};

/// The hub's own baked config, parsed from the very file `generate_context!`
/// compiles in — so these tests start from the identity a running hub really
/// carries, not from a hand-built stand-in that could drift away from it.
fn baked_config() -> Config {
    serde_json::from_str(include_str!("../tauri.conf.json")).expect("hub tauri.conf.json parses")
}

fn test_identity() -> Identity {
    Identity {
        identifier: "dev.tfsapp.test".to_string(),
        product_name: "TFS App Test".to_string(),
        icon_path: None,
    }
}

#[test]
fn overrides_the_hub_own_baked_identity() {
    let mut config = baked_config();
    assert_eq!(config.identifier, "dev.tfsapp.hub");

    apply_to_config(&test_identity(), &mut config);

    assert_eq!(config.identifier, "dev.tfsapp.test");
    assert_eq!(config.product_name.as_deref(), Some("TFS App Test"));
}

#[test]
fn enables_the_gtk_app_id_the_baked_config_leaves_off() {
    let mut config = baked_config();
    // Asserted before the mutation on purpose: the day someone adds
    // `enableGtkAppId` to tauri.conf.json, this fails and says so, rather than
    // letting the explicit set below become a silent no-op that nothing would
    // notice going missing.
    assert!(!config.app.enable_gtk_app_id);

    apply_to_config(&test_identity(), &mut config);

    assert!(config.app.enable_gtk_app_id);
}

#[test]
fn decodes_a_png_from_a_runtime_path() {
    // Any PNG on disk will do; the hub's own is simply the one guaranteed to
    // be there. What this really asserts is that `tauri`'s `image-png` feature
    // is still enabled — drop it from Cargo.toml and every app silently loses
    // its icon, with nothing else in the workspace noticing.
    let icon = load_icon(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/icons/icon.png"
    )));

    assert!(icon.is_some_and(|image| image.width() > 0 && image.height() > 0));
}

#[test]
fn a_missing_icon_is_not_an_error() {
    // An app whose author shipped a broken icon path must still open.
    assert!(load_icon(Path::new("/nonexistent/icon.png")).is_none());
}
