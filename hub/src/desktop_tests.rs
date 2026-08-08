use std::path::{Path, PathBuf};

use super::{carries_marker, remove, render, write, RemovalOutcome};
use crate::{identity::Identity, paths::Paths};

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

fn identity(identifier: &str, product_name: &str, icon_path: Option<PathBuf>) -> Identity {
    Identity {
        identifier: identifier.to_string(),
        product_name: product_name.to_string(),
        icon_path,
    }
}

#[test]
fn the_rendered_entry_matches_the_specification_field_for_field() {
    let entity = identity(
        "dev.local.tfsapp-test",
        "TFSApp Test",
        Some(PathBuf::from("/opt/apps/tfsapp-test/icon.png")),
    );

    let rendered = render(
        "tfsapp-test",
        &entity,
        Path::new("/home/user/.local/share/TFSApp/hub/bin/tfsapp-hub"),
    );

    assert_eq!(
        rendered,
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=TFSApp\\sTest\n\
         Exec=\"/home/user/.local/share/TFSApp/hub/bin/tfsapp-hub\" open tfsapp-test\n\
         Icon=/opt/apps/tfsapp-test/icon.png\n\
         Terminal=false\n\
         StartupWMClass=dev.local.tfsapp-test\n\
         X-TFSApp-Id=tfsapp-test\n"
    );
}

#[test]
fn name_is_sourced_from_the_manifest_identity_and_nothing_else() {
    let loaded = crate::manifest::parse(
        Path::new("tfsapp.config.json"),
        r#"{
            "product_name": "LabelBoard",
            "identifier": "dev.local.labelboard",
            "project_name": "labelboard",
            "app_version": "1.0.0"
        }"#,
    )
    .expect("a valid manifest");
    let app_dir = Path::new("/opt/apps/labelboard");
    let identity = loaded.manifest.identity(app_dir);

    let rendered = render("labelboard", &identity, Path::new("/hub/tfsapp-hub"));

    assert!(rendered.contains("Name=LabelBoard\n"));
    assert_eq!(identity.product_name, "LabelBoard");
}

#[test]
fn no_icon_declared_means_no_icon_line() {
    let entity = identity("dev.local.tfsapp-test", "TFSApp Test", None);

    let rendered = render("tfsapp-test", &entity, Path::new("/hub/tfsapp-hub"));

    assert!(!rendered.contains("Icon="));
}

#[test]
fn a_product_name_with_a_quote_and_a_newline_is_escaped_but_not_quoted() {
    let entity = identity("dev.local.tfsapp-test", "The \"Best\" App\nEver", None);

    let rendered = render("tfsapp-test", &entity, Path::new("/hub/tfsapp-hub"));

    // The quote survives unescaped — a desktop entry value is not itself
    // quoted, so `"` needs no escaping. The newline must become `\n`, or the
    // file would no longer be one line per key, and every space becomes `\s`
    // along with it.
    assert!(rendered.contains("Name=The\\s\"Best\"\\sApp\\nEver\n"));
}

#[test]
fn a_hub_path_with_a_space_and_a_percent_is_quoted_and_doubled() {
    let entity = identity("dev.local.tfsapp-test", "TFSApp Test", None);

    let rendered = render(
        "tfsapp-test",
        &entity,
        Path::new("/home/a b/tfsapp hub/tfsapp-hub%"),
    );

    assert!(rendered.contains("Exec=\"/home/a b/tfsapp hub/tfsapp-hub%%\" open tfsapp-test\n"));
}

#[test]
fn the_marker_is_present_and_matches_carries_marker() {
    let entity = identity("dev.local.tfsapp-test", "TFSApp Test", None);

    let rendered = render("tfsapp-test", &entity, Path::new("/hub/tfsapp-hub"));

    assert!(rendered.contains("X-TFSApp-Id=tfsapp-test\n"));
    assert!(carries_marker(&rendered, "tfsapp-test"));
    assert!(!carries_marker(&rendered, "some-other-app"));
}

#[test]
fn write_then_remove_round_trips_and_the_file_is_gone() {
    let (_base, paths) = temp_paths();
    let entity = identity("dev.local.tfsapp-test", "TFSApp Test", None);

    let path = write("tfsapp-test", &entity, Path::new("/hub/tfsapp-hub"), &paths)
        .expect("the entry is written");
    assert!(path.is_file());

    let outcome =
        remove("tfsapp-test", &entity.identifier, &paths).expect("removal does not error");

    assert_eq!(outcome, RemovalOutcome::Removed);
    assert!(!path.exists());
}

#[test]
fn removing_an_entry_that_was_never_written_is_reported_absent() {
    let (_base, paths) = temp_paths();

    let outcome =
        remove("tfsapp-test", "dev.local.tfsapp-test", &paths).expect("removal does not error");

    assert_eq!(outcome, RemovalOutcome::Absent);
}

#[test]
fn a_foreign_file_with_no_marker_is_left_alone() {
    let (_base, paths) = temp_paths();
    let identifier = "dev.local.tfsapp-test";
    let path = paths
        .desktop_entry_path(identifier)
        .expect("a safe identifier");
    std::fs::create_dir_all(paths.applications_dir()).expect("the applications dir");
    std::fs::write(
        &path,
        "[Desktop Entry]\nType=Application\nName=Hand Written\n",
    )
    .expect("a hand-written entry");

    let outcome = remove("tfsapp-test", identifier, &paths).expect("removal does not error");

    assert_eq!(outcome, RemovalOutcome::LeftAlone);
    assert!(path.is_file(), "a foreign entry must survive removal");
}
