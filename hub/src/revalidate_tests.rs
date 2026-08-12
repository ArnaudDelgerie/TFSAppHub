use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use super::{revalidate, Outcome};
use crate::{
    paths::Paths,
    php, platform,
    registry::{self, now_timestamp, Platform, RegistryEntry, Source, SourceKind, State},
};

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

/// A minimal app tree, with `composer_require` spliced into `composer.json`'s
/// `require` object — empty for one Composer resolves at once, or a
/// requirement no PHP will ever satisfy for one it cannot.
fn app_tree(root: &Path, composer_require: &str) {
    fs::create_dir_all(root).expect("a project root");
    fs::write(
        root.join("tfsapp.config.json"),
        r#"{
          "product_name": "Demo App",
          "identifier": "dev.local.demo",
          "project_name": "demo",
          "app_version": "0.6.0"
        }"#,
    )
    .expect("a manifest");
    fs::write(
        root.join("composer.json"),
        format!("{{\"require\": {{{composer_require}}}}}"),
    )
    .expect("a composer.json");

    fs::create_dir_all(root.join("bin")).expect("a bin dir");
    fs::write(root.join("bin/console"), "#!/usr/bin/env php\n").expect("a console");
    fs::set_permissions(root.join("bin/console"), fs::Permissions::from_mode(0o755))
        .expect("an executable console");

    fs::create_dir_all(root.join("public")).expect("a public dir");
    fs::write(root.join("public/index.php"), "<?php").expect("a front controller");
}

fn manifest(root: &Path) -> crate::manifest::Manifest {
    crate::manifest::load(root)
        .expect("a readable manifest")
        .manifest
}

/// A fingerprint the real probe could never answer — the "last known" platform
/// a seeded entry carries in, so a test can tell whether [`revalidate`] left it
/// alone (the `Broken` case) or advanced it (the `Ready` case).
fn implausible_platform() -> Platform {
    Platform {
        php_version: "0.0".to_string(),
        extensions_hash: "not-a-real-probe".to_string(),
    }
}

fn seeded_entry() -> RegistryEntry {
    RegistryEntry {
        id: "demo".to_string(),
        identifier: "dev.local.demo".to_string(),
        source: Source {
            kind: SourceKind::LocalPath,
            location: "/home/arnaud/Dev/demo".to_string(),
            reference: None,
            reference_kind: None,
            index: None,
        },
        app_version: "0.6.0".to_string(),
        source_revision: "sha256:deadbeef".to_string(),
        app_port: None,
        platform: implausible_platform(),
        state: State::NeedsRevalidation,
        installed_at: now_timestamp(),
        updated_at: now_timestamp(),
        unknown: serde_json::Map::new(),
    }
}

/// The bundled interpreter and Composer, or a reason to skip — the same gate
/// `install_tests.rs` uses, duplicated rather than imported: each `_tests.rs`
/// file is self-contained, and a 170 MB download is not something a unit test
/// should trigger.
fn resources_present() -> bool {
    let missing: Vec<_> = [platform::bundled_frankenphp(), php::bundled_composer()]
        .into_iter()
        .filter(|candidates| !candidates.iter().any(|path| path.is_file()))
        .collect();

    for candidates in &missing {
        eprintln!(
            "skipped: none of {candidates:?} are there — run `make resources` to cover this one"
        );
    }
    missing.is_empty()
}

#[test]
fn a_satisfiable_lock_ends_ready_with_the_new_platform_stamped() {
    if !resources_present() {
        return;
    }
    let (_base, paths) = temp_paths();
    let source = tempfile::tempdir().expect("a temp source");
    app_tree(source.path(), "");
    let manifest = manifest(source.path());

    registry::update(&paths, |registry| registry.upsert(seeded_entry())).expect("a seeded entry");

    let outcome =
        revalidate(&paths, "demo", source.path(), &manifest).expect("composer resolves cleanly");
    assert!(matches!(outcome, Outcome::Ready(_)), "{outcome:?}");

    let after = registry::load(&paths).expect("it reads");
    let entry = after.get("demo").expect("still there");
    assert_eq!(entry.state, State::Ready);
    assert_eq!(
        entry.platform,
        platform::hub_platform().expect("a probe of the same interpreter")
    );
}

#[test]
fn an_unsatisfiable_requirement_ends_broken_and_keeps_the_last_known_platform() {
    if !resources_present() {
        return;
    }
    let (_base, paths) = temp_paths();
    let source = tempfile::tempdir().expect("a temp source");
    // No PHP will ever be 99.x — Composer refuses on the platform check alone,
    // with no package resolution or network involved.
    app_tree(source.path(), "\"php\": \"^99.0\"");
    let manifest = manifest(source.path());

    registry::update(&paths, |registry| registry.upsert(seeded_entry())).expect("a seeded entry");

    let outcome = revalidate(&paths, "demo", source.path(), &manifest)
        .expect("a composer failure is Ok(Broken), not an Err");
    assert!(matches!(outcome, Outcome::Broken), "{outcome:?}");

    let after = registry::load(&paths).expect("it reads");
    let entry = after.get("demo").expect("still there");
    assert_eq!(entry.state, State::Broken);
    // Left exactly as seeded: overwriting it with the platform that just
    // failed would say the opposite of what `OpenError::Broken` means.
    assert_eq!(entry.platform, implausible_platform());
}

#[test]
fn a_platform_change_mismatches_the_stale_cache_stamp_on_the_fingerprint_alone() {
    // Plan 024 step 5, invalidation path 3: a hub self-update moves PHP, marks
    // the entry `NeedsRevalidation`, and the next `open` calls `revalidate`
    // — which never touches `cache.json` itself (unlike `update`'s revert or
    // `install::prepare`'s own warm-up). The stale stamp a prior install left,
    // naming the *old* platform, must mismatch against the stamp the next
    // launch would build from the freshly probed one — on the platform
    // dimension alone, with `app_version`/`snapshot_path` untouched.
    if !resources_present() {
        return;
    }
    let (base, paths) = temp_paths();
    let source = tempfile::tempdir().expect("a temp source");
    app_tree(source.path(), "");
    let manifest = manifest(source.path());

    registry::update(&paths, |registry| registry.upsert(seeded_entry())).expect("a seeded entry");

    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    let stale_stamp = crate::lifecycle::CacheStamp {
        app_version: manifest.app_version.clone(),
        snapshot_path: tfsapp_core::sidecar::path_to_string(source.path()),
        platform: implausible_platform(),
    };
    crate::lifecycle::write_cache_stamp(&data_subdir, &stale_stamp).expect("a seeded stamp");

    let outcome =
        revalidate(&paths, "demo", source.path(), &manifest).expect("composer resolves cleanly");
    let Outcome::Ready(fresh_platform) = outcome else {
        panic!("expected Ready, got {outcome:?}");
    };
    assert_ne!(
        fresh_platform,
        implausible_platform(),
        "the real probe can never answer the seeded, implausible platform"
    );

    let expected = crate::lifecycle::CacheStamp {
        app_version: manifest.app_version.clone(),
        snapshot_path: tfsapp_core::sidecar::path_to_string(source.path()),
        platform: fresh_platform,
    };
    let cache_dir = base.path().join("TFSApp/dev.local.demo/cache");

    match crate::lifecycle::read_cache_stamp(&data_subdir, &cache_dir, &expected) {
        crate::lifecycle::CacheStatus::Mismatch { reason } => {
            assert!(
                reason.contains("the platform changed"),
                "the mismatch must be on the platform dimension alone: {reason}"
            );
        }
        other => panic!("expected a mismatch on the stale stamp's platform, got {other:?}"),
    }
}

#[test]
fn an_entry_removed_out_from_under_a_revalidation_is_not_an_error() {
    if !resources_present() {
        return;
    }
    let (_base, paths) = temp_paths();
    let source = tempfile::tempdir().expect("a temp source");
    app_tree(source.path(), "");
    let manifest = manifest(source.path());
    // No entry seeded at all — the `remove`-raced-`open` case from this
    // function's own doc comment.

    let outcome = revalidate(&paths, "demo", source.path(), &manifest)
        .expect("a missing entry is not an error here");
    assert!(matches!(outcome, Outcome::Ready(_)), "{outcome:?}");

    let after = registry::load(&paths).expect("it reads");
    assert!(after.get("demo").is_none());
}
