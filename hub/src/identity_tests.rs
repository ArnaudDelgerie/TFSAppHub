use std::path::Path;

use tauri::utils::config::Config;

use super::{apply_to_config, fit_to_limit, load_icon, Identity, MAX_ICON_WORDS};

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

#[test]
fn an_icon_that_already_fits_is_left_exactly_as_it_came() {
    let rgba = vec![7u8; 4 * 4 * 4];

    let (fitted, width, height) = fit_to_limit(rgba.clone(), 4, 4, MAX_ICON_WORDS);

    assert_eq!((width, height), (4, 4));
    assert_eq!(fitted, rgba);
}

#[test]
fn an_oversized_icon_is_box_averaged_down() {
    // A 4×4 image under a cap of 6 words (2 + 2×2) forces factor 2. Rows are
    // 0, 10, 20, 30 repeated across every channel, so each output pixel must
    // be the mean of its own 2×2 block: (0 + 0 + 10 + 10) / 4 = 5 on the top
    // row, (20 + 20 + 30 + 30) / 4 = 25 on the bottom.
    let rgba: Vec<u8> = [0u8, 10, 20, 30]
        .iter()
        .flat_map(|row| std::iter::repeat_n(*row, 4 * 4))
        .collect();

    let (fitted, width, height) = fit_to_limit(rgba, 4, 4, 6);

    assert_eq!((width, height), (2, 2));
    assert_eq!(
        fitted,
        vec![5, 5, 5, 5, 5, 5, 5, 5, 25, 25, 25, 25, 25, 25, 25, 25]
    );
}

#[test]
fn a_ragged_edge_averages_the_smaller_block_that_is_there() {
    // 3×1 under a cap of 4 words: factor 2 gives a 2×1 target whose second
    // block holds a single source pixel. Averaging by the block that exists,
    // not by the block that was asked for, is what keeps that pixel intact.
    let rgba = vec![
        0, 0, 0, 0, // (0,0)
        100, 100, 100, 100, // (1,0)
        200, 200, 200, 200, // (2,0)
    ];

    let (fitted, width, height) = fit_to_limit(rgba, 3, 1, 4);

    assert_eq!((width, height), (2, 1));
    assert_eq!(fitted, vec![50, 50, 50, 50, 200, 200, 200, 200]);
}

#[test]
fn the_hub_own_512_icon_comes_back_at_the_largest_size_x11_carries() {
    // The end-to-end shape of the bug step 4 measured: this very PNG produced
    // no `_NET_WM_ICON` at all before `fit_to_limit` existed.
    let icon = load_icon(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/icons/icon.png"
    )))
    .expect("the hub's own icon decodes");

    assert_eq!((icon.width(), icon.height()), (256, 256));
    assert!(super::fits(icon.width(), icon.height(), MAX_ICON_WORDS));
}
