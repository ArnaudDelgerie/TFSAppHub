use std::{fs, path::Path};

use super::{
    discard_tree, restore_tree, retain_tree, update, update_decision, UpdateAction, UpdateError,
    UpdateRefusal,
};
use crate::{
    lifecycle::{previous_tree_path, read_rollback_anchor},
    paths::Paths,
    registry::{self, Platform, RegistryEntry, Source, SourceKind, State},
};

fn version(text: &str) -> semver::Version {
    semver::Version::parse(text).expect("a semver version")
}

// --- update_decision -------------------------------------------------------

#[test]
fn a_newer_source_applies_the_update_event() {
    assert_eq!(
        update_decision(Some("0.5.0"), &version("0.6.0"), false),
        Ok(UpdateAction::Apply)
    );
}

#[test]
fn a_newer_source_applies_regardless_of_force() {
    // `--force` is for the equal case only — a real update needs no unlocking.
    assert_eq!(
        update_decision(Some("0.5.0"), &version("0.6.0"), true),
        Ok(UpdateAction::Apply)
    );
}

#[test]
fn an_equal_source_refuses_without_force() {
    let error = update_decision(Some("0.6.0"), &version("0.6.0"), false)
        .expect_err("an equal record refuses without --force");
    assert_eq!(
        error,
        UpdateRefusal::Equal {
            version: version("0.6.0")
        }
    );
}

#[test]
fn an_equal_source_with_force_resyncs_only() {
    assert_eq!(
        update_decision(Some("0.6.0"), &version("0.6.0"), true),
        Ok(UpdateAction::ResyncOnly)
    );
}

#[test]
fn a_downgrade_refuses_whether_or_not_force_is_given() {
    // `--force` is for the case `source_revision` exists to catch — a tree
    // edited without bumping the version — never for reverting to an older
    // release.
    for force in [false, true] {
        let error = update_decision(Some("0.6.0"), &version("0.5.0"), force)
            .expect_err("a downgrade always refuses");
        assert_eq!(
            error,
            UpdateRefusal::Downgrade {
                recorded: version("0.6.0"),
                source: version("0.5.0"),
            }
        );
    }
}

#[test]
fn no_record_refuses_as_an_install_whether_or_not_force_is_given() {
    // A missing record with `--force` is still an install: `update` never
    // adopts the install event, whatever flag is passed.
    for force in [false, true] {
        let error = update_decision(None, &version("0.6.0"), force)
            .expect_err("no record at all is install's, not update's");
        assert_eq!(error, UpdateRefusal::NoRecord);
    }
}

#[test]
fn an_unparseable_recorded_version_is_reported_rather_than_panicking() {
    let error = update_decision(Some("not-a-version"), &version("0.6.0"), false)
        .expect_err("an unparseable record cannot be compared");
    assert!(matches!(error, UpdateRefusal::InvalidRecordedVersion(_)));
}

// --- the tree-anchor helpers -------------------------------------------------

fn app_tree(root: &std::path::Path, marker: &str) {
    fs::create_dir_all(root).expect("an app dir");
    fs::write(root.join("marker"), marker).expect("a marker file");
}

#[test]
fn retaining_a_tree_renames_it_to_its_previous_sibling() {
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    app_tree(&app_dir, "outgoing");

    retain_tree(&app_dir).expect("a retain");

    assert!(!app_dir.exists(), "the tree was renamed, not copied");
    let previous = previous_tree_path(&app_dir);
    assert_eq!(
        fs::read_to_string(previous.join("marker")).expect("the retained marker"),
        "outgoing"
    );
}

#[test]
fn retaining_a_tree_replaces_a_leftover_previous_from_an_interrupted_earlier_update() {
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    app_tree(&app_dir, "this update's outgoing tree");
    // A stale `.previous` from an update that was interrupted before its own
    // anchor was ever consumed.
    app_tree(
        &previous_tree_path(&app_dir),
        "an earlier, already-abandoned tree",
    );

    retain_tree(&app_dir).expect("a retain");

    let previous = previous_tree_path(&app_dir);
    assert_eq!(
        fs::read_to_string(previous.join("marker")).expect("the retained marker"),
        "this update's outgoing tree",
        "the stale .previous must be replaced, not appended to"
    );
}

#[test]
fn restoring_a_tree_renames_previous_back_to_the_app_dir() {
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    app_tree(&previous_tree_path(&app_dir), "retained");

    restore_tree(&app_dir).expect("a restore");

    assert!(!previous_tree_path(&app_dir).exists());
    assert_eq!(
        fs::read_to_string(app_dir.join("marker")).expect("the restored marker"),
        "retained"
    );
}

#[test]
fn discarding_a_tree_removes_previous_without_touching_the_app_dir() {
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    app_tree(&app_dir, "current");
    app_tree(&previous_tree_path(&app_dir), "abandoned");

    discard_tree(&app_dir);

    assert!(!previous_tree_path(&app_dir).exists());
    assert_eq!(
        fs::read_to_string(app_dir.join("marker")).expect("the untouched marker"),
        "current"
    );
}

#[test]
fn discarding_an_absent_previous_tree_is_not_an_error() {
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    discard_tree(&app_dir); // must not panic
}

// --- the `update <id>` command --------------------------------------------

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

/// A registry entry recording `dev.local.demo` at `0.6.0`, sourced from
/// `location` — the state a real `install` would have left behind, without
/// paying for one.
fn seeded_entry(location: &str) -> RegistryEntry {
    RegistryEntry {
        id: "demo".to_string(),
        identifier: "dev.local.demo".to_string(),
        source: Source {
            kind: SourceKind::LocalPath,
            location: location.to_string(),
            reference: None,
            reference_kind: None,
            index: None,
        },
        app_version: "0.6.0".to_string(),
        source_revision: "sha256:deadbeef".to_string(),
        app_port: None,
        platform: Platform {
            php_version: "8.5".to_string(),
            extensions_hash: "a1b2c3d4".repeat(8),
        },
        state: State::Ready,
        installed_at: registry::now_timestamp(),
        updated_at: registry::now_timestamp(),
        unknown: serde_json::Map::new(),
    }
}

/// The smallest tree `install::validate` accepts, with no lifecycle commands
/// declared — enough to test the refusals, which never reach a PHP process.
fn minimal_app_tree(root: &Path, version: &str) {
    fs::create_dir_all(root).expect("a project root");
    fs::write(
        root.join("tfsapp.config.json"),
        format!(
            r#"{{
              "product_name": "Demo App",
              "identifier": "dev.local.demo",
              "project_name": "demo",
              "app_version": "{version}"
            }}"#
        ),
    )
    .expect("a manifest");
    fs::write(root.join("composer.json"), "{\"require\": {}}").expect("a composer.json");

    fs::create_dir_all(root.join("bin")).expect("a bin dir");
    fs::write(root.join("bin/console"), "#!/usr/bin/env php\n").expect("a console");

    fs::create_dir_all(root.join("public")).expect("a public dir");
    fs::write(root.join("public/index.php"), "<?php").expect("a front controller");
}

#[test]
fn updating_an_unregistered_id_refuses() {
    let (_base, paths) = temp_paths();

    let error =
        update(&paths, "demo", None, false, true, "0.1.0").expect_err("nothing is installed");
    assert!(matches!(error, UpdateError::NotInstalled { .. }), "{error}");
}

#[test]
fn a_source_that_no_longer_exists_is_refused_naming_the_recorded_location() {
    let (base, paths) = temp_paths();
    let gone = base.path().join("gone");
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry(&gone.display().to_string()))
    })
    .expect("a seeded registry");

    let error = update(&paths, "demo", None, false, true, "0.1.0")
        .expect_err("the recorded source directory is gone");
    assert!(matches!(error, UpdateError::Source(_)), "{error}");
    assert!(
        error.to_string().contains(&gone.display().to_string()),
        "{error}"
    );
}

#[test]
fn a_source_already_at_the_recorded_version_refuses_without_force() {
    let (_base, paths) = temp_paths();
    let source = tempfile::tempdir().expect("a temp source");
    minimal_app_tree(source.path(), "0.6.0");
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry(&source.path().display().to_string()))
    })
    .expect("a seeded registry");

    let error = update(&paths, "demo", None, false, true, "0.1.0")
        .expect_err("an equal source refuses without --force");
    assert!(matches!(error, UpdateError::Equal { .. }), "{error}");
}

#[test]
fn a_downgrade_refuses_naming_both_versions() {
    let (_base, paths) = temp_paths();
    let source = tempfile::tempdir().expect("a temp source");
    minimal_app_tree(source.path(), "0.5.0"); // older than the recorded 0.6.0
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry(&source.path().display().to_string()))
    })
    .expect("a seeded registry");

    let error =
        update(&paths, "demo", None, false, true, "0.1.0").expect_err("a downgrade refuses");
    assert!(matches!(error, UpdateError::Downgrade { .. }), "{error}");
}

/// The bundled interpreter and Composer, or a reason to skip — the same gate
/// `install_tests.rs` uses, duplicated rather than imported: each `_tests.rs`
/// file is self-contained, and a 170 MB download is not something a unit test
/// should trigger.
fn resources_present() -> bool {
    let missing: Vec<_> = [
        crate::platform::bundled_frankenphp(),
        crate::php::bundled_composer(),
    ]
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

/// A fixture app whose `bin/console` records what it was asked to do, and
/// fails on the word `boom` — `install_tests.rs`'s own `runnable_app_tree`,
/// parameterised on `version` since this module rewrites the manifest in
/// place to simulate a newer source landing.
fn runnable_app_tree(root: &Path, version: &str, commands: &str) {
    fs::create_dir_all(root).expect("a project root");
    fs::write(
        root.join("tfsapp.config.json"),
        format!(
            r#"{{
              "product_name": "Demo App",
              "identifier": "dev.local.demo",
              "project_name": "demo",
              "app_version": "{version}",
              "commands": {commands}
            }}"#
        ),
    )
    .expect("a manifest");
    fs::write(root.join("composer.json"), "{}").expect("a composer.json");

    fs::create_dir_all(root.join("bin")).expect("a bin dir");
    fs::write(
        root.join("bin/console"),
        r#"<?php
        $arguments = array_slice($argv, 1);
        file_put_contents(
            getenv('APP_LOG_DIR') . '/hooks.log',
            implode(' ', $arguments) . "\n",
            FILE_APPEND
        );
        exit(in_array('boom', $arguments, true) ? 1 : 0);
        "#,
    )
    .expect("a console");

    fs::create_dir_all(root.join("public")).expect("a public dir");
    fs::write(root.join("public/index.php"), "<?php").expect("a front controller");
}

#[test]
fn an_update_runs_pre_update_then_post_update_and_no_install_hooks() {
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();

    runnable_app_tree(
        source.path(),
        "0.6.0",
        r#"{"pre-install": ["about"], "post-install": ["doctrine:migrations:migrate"]}"#,
    );
    crate::install::install(
        &paths,
        &source.path().display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");

    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::write(data_subdir.join("app.db"), b"pre-update-bytes").expect("a seeded database");

    // A newer source, with update hooks instead of install ones.
    runnable_app_tree(
        source.path(),
        "0.7.0",
        r#"{"pre-update": ["cache:clear"], "post-update": ["about"]}"#,
    );

    update(&paths, "demo", None, false, true, "0.1.0").expect("the update applies");

    let log = base.path().join("TFSApp/dev.local.demo/log/hooks.log");
    // The hub's own `cache:warmup` (plan 024) closes out each event — the
    // install's, then the update's — through this same fixture console.
    assert_eq!(
        fs::read_to_string(log).expect("a hook trace"),
        "about\ndoctrine:migrations:migrate\ncache:warmup --env=prod --no-debug\n\
         cache:clear\nabout\ncache:warmup --env=prod --no-debug\n",
        "the install's own hooks must not repeat, and the update's must run in order"
    );

    let updated = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry survives");
    assert_eq!(updated.app_version, "0.7.0");
    let anchor = read_rollback_anchor(&data_subdir).expect("a complete rollback anchor");
    assert_eq!(anchor.app_version, "0.6.0");
    assert!(
        previous_tree_path(&paths.app_dir("demo").expect("an app dir")).is_dir(),
        "a successful update keeps the outgoing tree"
    );
    assert!(data_subdir.join("app.db.pre-update").is_file());
}

#[test]
fn a_failing_pre_update_leaves_the_tree_the_database_and_the_registry_entry_unchanged() {
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();

    runnable_app_tree(source.path(), "0.6.0", "{}");
    crate::install::install(
        &paths,
        &source.path().display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");

    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::write(data_subdir.join("app.db"), b"pre-update-bytes").expect("a seeded database");

    let app_dir = paths.app_dir("demo").expect("an app dir");
    let before_manifest =
        fs::read_to_string(app_dir.join("tfsapp.config.json")).expect("the outgoing manifest");
    let before_db = fs::read(data_subdir.join("app.db")).expect("the outgoing database");
    let before_registry = registry::load(&paths).expect("the outgoing registry");

    runnable_app_tree(source.path(), "0.7.0", r#"{"pre-update": ["boom"]}"#);

    let error = update(&paths, "demo", None, false, true, "0.1.0")
        .expect_err("a failing pre-update must fail the whole update");
    assert!(matches!(error, UpdateError::Reverted { .. }), "{error}");

    assert_eq!(
        fs::read_to_string(app_dir.join("tfsapp.config.json")).expect("the restored manifest"),
        before_manifest,
        "the tree must come back exactly as it was"
    );
    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the restored database"),
        before_db,
        "the database must come back exactly as it was"
    );
    assert_eq!(
        registry::load(&paths).expect("the untouched registry"),
        before_registry,
        "the registry must never be touched on a reverted update"
    );
    assert!(
        !previous_tree_path(&app_dir).exists(),
        "a reverted update leaves no anchor behind"
    );
    assert!(
        !data_subdir.join("app.db.pre-update").exists(),
        "a reverted update consumes the database half of its anchor"
    );
    assert!(
        read_rollback_anchor(&data_subdir).is_none(),
        "a reverted update consumes the anchor record too"
    );
}

#[test]
fn an_update_leaves_a_cache_stamp_the_next_launch_will_match() {
    // Plan 024 step 5, invalidation path 1: an update to a new `app_version`
    // must leave a stamp naming that new version — the one `open::resolve`
    // will build as `expected` on the very next launch.
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();

    runnable_app_tree(source.path(), "0.6.0", "{}");
    crate::install::install(
        &paths,
        &source.path().display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");

    runnable_app_tree(source.path(), "0.7.0", "{}");
    update(&paths, "demo", None, false, true, "0.1.0").expect("the update applies");

    let app_dir = paths.app_dir("demo").expect("an app dir");
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join("data");
    let cache_dir = data_dir.join("cache");
    // A real launch always finds `cache/` repopulated by its own warm-up —
    // this fixture's console never actually writes one, so it is faked here
    // to isolate what this test cares about: whether the stamp's *fields*
    // would let a launch reuse it, not whether this fixture writes a cache.
    fs::create_dir_all(&cache_dir).expect("a cache dir");
    fs::write(cache_dir.join("marker"), b"warm").expect("a cache entry");

    let entry = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry survives");
    let expected = crate::lifecycle::CacheStamp {
        app_version: entry.app_version.clone(),
        snapshot_path: tfsapp_core::sidecar::path_to_string(&app_dir),
        platform: entry.platform.clone(),
    };

    assert_eq!(
        crate::lifecycle::read_cache_stamp(&data_subdir, &cache_dir, &expected),
        crate::lifecycle::CacheStatus::Matches,
        "an update's own warm-up must leave a stamp the very next launch reuses"
    );
}

#[test]
fn force_on_an_equal_source_resyncs_without_running_any_hook() {
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();

    runnable_app_tree(
        source.path(),
        "0.6.0",
        r#"{"pre-update": ["cache:clear"], "post-update": ["about"]}"#,
    );
    crate::install::install(
        &paths,
        &source.path().display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");

    let before_registry = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry");

    // A developer edits the source without bumping the version — a different
    // `source_revision`, same `app_version`.
    fs::write(source.path().join("README"), "edited").expect("an edit");

    update(&paths, "demo", None, true, true, "0.1.0").expect("--force resyncs");

    let log = base.path().join("TFSApp/dev.local.demo/log/hooks.log");
    // The initial install ran no declared hook (no `pre-install`/`post-install`
    // in this manifest) but still ran the hub's own `cache:warmup` (plan 024);
    // a resync goes through `resync_only`, never `install::prepare`, so it
    // must add nothing further to this trace.
    assert_eq!(
        fs::read_to_string(log).expect("a hook trace"),
        "cache:warmup --env=prod --no-debug\n",
        "a resync must run no lifecycle command, and must not warm the cache a second time"
    );
    assert!(
        !previous_tree_path(&paths.app_dir("demo").expect("an app dir")).exists(),
        "a resync must not rotate the anchor"
    );

    let after_registry = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry");
    assert_eq!(after_registry.app_version, before_registry.app_version);
    assert_ne!(
        after_registry.source_revision, before_registry.source_revision,
        "a resync must still catch the source's own change"
    );
}

// --- update on a remote source ----------------------------------------------
//
// `install_tests.rs`'s own `fixture_release_archive`/`stub_release`,
// duplicated rather than imported — each `_tests.rs` file is self-contained.

fn fixture_release_archive(top_level: &str, app_version: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));

    let mut dir_header = tar::Header::new_gnu();
    dir_header.set_entry_type(tar::EntryType::Directory);
    dir_header.set_size(0);
    dir_header.set_mode(0o755);
    dir_header
        .set_path(format!("{top_level}/"))
        .expect("a dir path");
    dir_header.set_cksum();
    builder
        .append(&dir_header, std::io::empty())
        .expect("append the top-level dir");

    let manifest = format!(
        r#"{{
            "product_name": "Demo App",
            "identifier": "dev.local.demo",
            "project_name": "demo",
            "app_version": "{app_version}"
        }}"#
    );
    append_archive_file(
        &mut builder,
        &format!("{top_level}/tfsapp.config.json"),
        manifest.as_bytes(),
    );
    append_archive_file(&mut builder, &format!("{top_level}/composer.json"), b"{}");
    append_archive_file(
        &mut builder,
        &format!("{top_level}/bin/console"),
        b"#!/usr/bin/env php\n",
    );
    append_archive_file(
        &mut builder,
        &format!("{top_level}/public/index.php"),
        b"<?php",
    );

    builder
        .into_inner()
        .expect("finish the tar layer")
        .finish()
        .expect("finish the gzip layer")
}

fn append_archive_file(
    builder: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>,
    path: &str,
    content: &[u8],
) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_path(path).expect("a valid file path");
    header.set_cksum();
    builder.append(&header, content).expect("append a file");
}

/// Answers the three requests resolving one release makes, in whatever order
/// they arrive, then stops.
fn stub_release(
    repo: &str,
    tag: &str,
    archive_name: &str,
    archive_bytes: Vec<u8>,
) -> (String, std::thread::JoinHandle<()>) {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(&archive_bytes);
    let checksums_body = format!("{:x}  {archive_name}\n", hasher.finalize());

    let server = tiny_http::Server::http("127.0.0.1:0").expect("a local stub server");
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(address) => address.port(),
        other => panic!("unexpected listen address: {other:?}"),
    };
    let base_url = format!("http://127.0.0.1:{port}");
    let release_path = format!("/repos/{repo}/releases/latest");
    let archive_path = format!("/assets/{archive_name}");
    let release_body = format!(
        r#"{{"tag_name": "{tag}", "html_url": "{base_url}/releases/tag/{tag}", "assets": [
            {{"name": "{archive_name}", "browser_download_url": "{base_url}{archive_path}", "size": {archive_size}}},
            {{"name": "SHA256SUMS.txt", "browser_download_url": "{base_url}/assets/SHA256SUMS.txt", "size": {sums_size}}}
        ]}}"#,
        archive_size = archive_bytes.len(),
        sums_size = checksums_body.len(),
    );

    let handle = std::thread::spawn(move || {
        for _ in 0..3 {
            let request = server.recv().expect("a request");
            let url = request.url().to_string();
            if url == release_path {
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .expect("a valid header");
                request
                    .respond(
                        tiny_http::Response::from_string(release_body.clone()).with_header(header),
                    )
                    .expect("respond with the release");
            } else if url == archive_path {
                request
                    .respond(tiny_http::Response::from_data(archive_bytes.clone()))
                    .expect("respond with the archive");
            } else if url == "/assets/SHA256SUMS.txt" {
                request
                    .respond(tiny_http::Response::from_string(checksums_body.clone()))
                    .expect("respond with the checksums");
            } else {
                request
                    .respond(tiny_http::Response::from_string("not found").with_status_code(404))
                    .expect("respond 404");
            }
        }
    });

    (base_url, handle)
}

#[test]
fn an_update_with_no_ref_moves_to_the_newest_remote_release() {
    if !resources_present() {
        return;
    }
    let (_base, paths) = temp_paths();
    let scratch = tempfile::tempdir().expect("a scratch dir");

    let first_archive = fixture_release_archive("demo-0.6.0", "0.6.0");
    let (first_url, first_handle) =
        stub_release("example/demo", "v0.6.0", "demo-0.6.0.tar.gz", first_archive);
    crate::install::install_into(
        &paths,
        scratch.path(),
        &first_url,
        "github:example/demo",
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");
    first_handle.join().expect("the first stub finishes");

    let second_archive = fixture_release_archive("demo-0.7.0", "0.7.0");
    let (second_url, second_handle) = stub_release(
        "example/demo",
        "v0.7.0",
        "demo-0.7.0.tar.gz",
        second_archive,
    );
    super::update_into(
        &paths,
        scratch.path(),
        &second_url,
        "demo",
        None,
        false,
        true,
        "0.1.0",
    )
    .expect("update moves to the newest release, no --ref needed");
    second_handle.join().expect("the second stub finishes");

    let entry = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry survives");
    assert_eq!(entry.app_version, "0.7.0");
    assert_eq!(entry.source.reference.as_deref(), Some("v0.7.0"));
}

#[test]
fn an_equal_remote_source_refuses_without_force() {
    // 017's decision table is source-kind-agnostic (`update_decision` never
    // reads `SourceKind`) — this is the one place that fires it end to end
    // over a *remote* source, to prove the table really did carry over
    // unchanged rather than merely being untested for this kind.
    if !resources_present() {
        return;
    }
    let (_base, paths) = temp_paths();
    let scratch = tempfile::tempdir().expect("a scratch dir");

    let archive = fixture_release_archive("demo-0.6.0", "0.6.0");
    let (install_url, install_handle) =
        stub_release("example/demo", "v0.6.0", "demo-0.6.0.tar.gz", archive);
    crate::install::install_into(
        &paths,
        scratch.path(),
        &install_url,
        "github:example/demo",
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the install");
    install_handle.join().expect("the install stub finishes");

    let same_archive = fixture_release_archive("demo-0.6.0", "0.6.0");
    let (update_url, update_handle) =
        stub_release("example/demo", "v0.6.0", "demo-0.6.0.tar.gz", same_archive);
    let error = super::update_into(
        &paths,
        scratch.path(),
        &update_url,
        "demo",
        None,
        false,
        true,
        "0.1.0",
    )
    .expect_err("the same release again refuses without --force");
    update_handle.join().expect("the update stub finishes");

    assert!(matches!(error, UpdateError::Equal { .. }), "{error}");
}
