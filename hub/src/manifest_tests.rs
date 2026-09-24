use std::path::{Path, PathBuf};

use super::{load, parse, ManifestError, MANIFEST_FILE};

/// Where a real manifest would be, so every message these tests assert on is
/// the one a user would actually read.
fn fixture_path() -> PathBuf {
    Path::new("/apps/tfsapp-test").join(MANIFEST_FILE)
}

fn parse_ok(contents: &str) -> super::Loaded {
    parse(&fixture_path(), contents).unwrap_or_else(|error| panic!("must parse: {error}"))
}

fn parse_err(contents: &str) -> ManifestError {
    parse(&fixture_path(), contents).expect_err("must not parse")
}

/// The minimum an app can declare: the four identity fields and nothing else.
const MINIMAL: &str = r#"{
  "product_name": "TFS App Test",
  "identifier": "dev.local.tfsapp-test",
  "project_name": "tfsapp-test",
  "app_version": "0.6.0"
}"#;

/// Modelled on TFSAppTest's own manifest, `pre-build`/`post-build` included —
/// those are build-machine keys the station's launcher ignores too, and an app
/// that carries them must not become unloadable in the hub.
const FULL: &str = r#"{
  "product_name": "TFS App Test",
  "identifier": "dev.local.tfsapp-test",
  "project_name": "tfsapp-test",
  "app_version": "0.6.0",
  "app_port": 8123,
  "icon_path": "assets/icon.png",
  "splash_path": "assets/splash.html",
  "async_worker": true,
  "commands": {
    "pre-install": ["doctrine:migrations:migrate --no-interaction"],
    "post-install": ["about"],
    "pre-update": ["doctrine:migrations:migrate --no-interaction"],
    "post-update": ["about"],
    "pre-build": ["composer install --no-dev"],
    "post-build": ["composer install"]
  },
  "releases_repo": "ArnaudDelgerie/TFSAppTest-releases",
  "run": {
    "mcp-serve": { "command": "app:run:mcp-serve", "concurrent": true },
    "cleanup": { "command": "app:run:cleanup" }
  },
  "actions": {
    "secrets": { "ipc": true, "bridge": true, "keys": ["openai", "anthropic"] },
    "update": { "ipc": true, "bridge": true },
    "picker": { "ipc": true },
    "close_guard": { "ipc": true, "bridge": true }
  }
}"#;

#[test]
fn a_full_manifest_parses_into_typed_values() {
    let loaded = parse_ok(FULL);
    let manifest = loaded.manifest;

    assert_eq!(manifest.product_name, "TFS App Test");
    assert_eq!(manifest.identifier, "dev.local.tfsapp-test");
    assert_eq!(manifest.project_name, "tfsapp-test");
    assert_eq!(manifest.app_version, "0.6.0");
    assert_eq!(manifest.app_port, Some(8123));
    assert_eq!(manifest.icon_path.as_deref(), Some("assets/icon.png"));
    assert_eq!(manifest.splash_path.as_deref(), Some("assets/splash.html"));
    assert!(manifest.async_worker);

    assert_eq!(
        manifest.commands.pre_install,
        ["doctrine:migrations:migrate --no-interaction"]
    );
    assert_eq!(manifest.commands.post_update, ["about"]);

    let cleanup = &manifest.run["cleanup"];
    assert_eq!(cleanup.command, "app:run:cleanup");
    // Absent `concurrent` is the restrictive default — standalone only.
    assert!(!cleanup.concurrent);
    assert!(manifest.run["mcp-serve"].concurrent);

    assert!(manifest.actions.secrets.ipc);
    assert_eq!(manifest.actions.secrets.keys, ["openai", "anthropic"]);
    assert!(manifest.actions.update.bridge);
    assert!(manifest.actions.picker.ipc);
    assert!(manifest.actions.close_guard.ipc);
    assert!(manifest.actions.close_guard.bridge);

    // `releases_repo` is a known key the hub does not read yet, and
    // `pre-build`/`post-build` are build-machine keys nested under `commands`.
    // Neither may produce a warning: the file is correct as written.
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
}

#[test]
fn a_minimal_manifest_defaults_everything_optional() {
    let loaded = parse_ok(MINIMAL);
    let manifest = loaded.manifest;

    assert_eq!(manifest.app_port, None);
    assert_eq!(manifest.icon_path, None);
    assert_eq!(manifest.splash_path, None);
    assert!(!manifest.async_worker);
    assert!(manifest.run.is_empty());
    assert_eq!(manifest.commands, super::LifecycleCommands::default());
    assert_eq!(manifest.actions, super::ActionsConfig::default());
    assert!(loaded.warnings.is_empty());
}

#[test]
fn picker_ipc_is_off_when_actions_or_picker_is_absent() {
    assert!(!parse_ok(MINIMAL).manifest.actions.picker.ipc);

    let without_picker = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "actions": {"update": {"ipc": true}}"#,
    );
    assert!(!parse_ok(&without_picker).manifest.actions.picker.ipc);
}

#[test]
fn picker_bridge_is_refused_even_when_false() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "actions": {"picker": {"ipc": true, "bridge": false}}"#,
    );

    let error = parse_err(&contents);

    assert!(matches!(
        error,
        ManifestError::UnsupportedActionTransport {
            group: "picker",
            transport: "bridge",
            ..
        }
    ));
    assert!(
        error.to_string().contains("actions.picker.bridge"),
        "{error}"
    );
    assert!(error.to_string().contains("no bridge transport"), "{error}");
}

#[test]
fn close_guard_is_off_at_every_level_and_neither_transport_implies_the_other() {
    assert_eq!(
        parse_ok(MINIMAL).manifest.actions.close_guard,
        super::CloseGuardActions::default()
    );

    let bridge_only = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "actions": {"close_guard": {"bridge": true}}"#,
    );
    let close_guard = parse_ok(&bridge_only).manifest.actions.close_guard;
    assert!(!close_guard.ipc);
    assert!(close_guard.bridge, "the bridge transport stands alone");

    let ipc_only = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "actions": {"close_guard": {"ipc": true}}"#,
    );
    let close_guard = parse_ok(&ipc_only).manifest.actions.close_guard;
    assert!(close_guard.ipc);
    assert!(!close_guard.bridge, "and so does the IPC transport");
}

#[test]
fn a_wrongly_typed_close_guard_transport_is_refused() {
    // The standing rule's corollary: a *known* key with the wrong type is
    // rejected rather than read as a default, and this group's switches are
    // exactly the shape a quoted "true" would silently invert.
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "actions": {"close_guard": {"ipc": "true"}}"#,
    );

    let error = parse_err(&contents);

    // The same refusal the other groups' switches already get: a *known* key
    // with the wrong type never reads as its default, and the file and the
    // offending value are named. The field name itself is the one thing
    // serde's error does not carry — which is why the top-level
    // `async_worker` has an explicit check and the nested group switches
    // never have.
    assert!(matches!(error, ManifestError::Invalid { .. }));
    assert!(error.to_string().contains("expected a boolean"), "{error}");
}

#[test]
fn an_unknown_top_level_key_warns_and_still_parses() {
    // The rule that lets the schema grow additively: a key this hub has never
    // heard of must not stop the app from being installed or opened.
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "app-port": 8123, "future_key": {"a": 1}"#,
    );

    let loaded = parse_ok(&contents);

    assert_eq!(loaded.manifest.app_version, "0.6.0");
    // The typo did not silently become `app_port`, which is the whole point of
    // warning about it.
    assert_eq!(loaded.manifest.app_port, None);
    assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
    assert!(loaded.warnings.iter().any(|w| w.contains("\"app-port\"")));
    assert!(loaded.warnings.iter().any(|w| w.contains("\"future_key\"")));
    // Every warning names the file, since with N apps installed the message
    // alone would not say which one.
    assert!(loaded
        .warnings
        .iter()
        .all(|w| w.contains("/apps/tfsapp-test/tfsapp.config.json")));
}

#[test]
fn broken_json_fails_with_the_file_and_the_position() {
    let error = parse_err("{ \"product_name\": \"TFS\", }");

    assert!(matches!(error, ManifestError::Malformed { .. }));
    let message = error.to_string();
    assert!(
        message.contains("/apps/tfsapp-test/tfsapp.config.json"),
        "{message}"
    );
    assert!(message.contains("line"), "{message}");
}

#[test]
fn a_manifest_that_is_not_an_object_says_what_it_found() {
    let error = parse_err("[]");

    let message = error.to_string();
    assert!(message.contains("must contain a JSON object"), "{message}");
    assert!(message.contains("an array"), "{message}");
}

#[test]
fn each_missing_identity_field_is_named() {
    for field in ["product_name", "identifier", "project_name", "app_version"] {
        let mut object: serde_json::Value = serde_json::from_str(MINIMAL).expect("valid fixture");
        object
            .as_object_mut()
            .expect("an object")
            .remove(field)
            .expect("the field was there");

        let error = parse_err(&object.to_string());

        assert!(
            matches!(&error, ManifestError::MissingField { field: missing, .. } if *missing == field),
            "unexpected error for a missing {field}: {error}"
        );
        assert!(error.to_string().contains(field), "{error}");
    }
}

#[test]
fn an_empty_identity_field_counts_as_missing() {
    // `build-app.sh` rejects a missing *or empty* `product_name` at build
    // time; the hub has no build step, so the same check has to live here.
    let contents = MINIMAL.replace(r#""TFS App Test""#, r#""   ""#);

    let error = parse_err(&contents);

    assert!(matches!(
        error,
        ManifestError::MissingField {
            field: "product_name",
            ..
        }
    ));
}

#[test]
fn an_identity_field_of_the_wrong_type_names_the_field_and_both_types() {
    let contents = MINIMAL.replace(r#""app_version": "0.6.0""#, r#""app_version": 0.6"#);

    let error = parse_err(&contents);

    let message = error.to_string();
    assert!(message.contains("\"app_version\""), "{message}");
    assert!(message.contains("must be a string"), "{message}");
    assert!(message.contains("found a number"), "{message}");
}

// --- async_worker, in its three states ------------------------------------
//
// The field the hub could most easily have got wrong in silence: read from the
// manifest here (like the station's dev mode), never from a bake (like its
// packaged mode), because the hub has no build step to bake anything.

#[test]
fn async_worker_true_is_read_from_the_manifest() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "async_worker": true"#,
    );

    assert!(parse_ok(&contents).manifest.async_worker);
}

#[test]
fn async_worker_absent_is_off() {
    assert!(!parse_ok(MINIMAL).manifest.async_worker);
}

#[test]
fn async_worker_of_the_wrong_type_is_a_parse_error_not_a_silent_false() {
    // A quoted boolean is the classic version of this typo, and defaulting it
    // to `false` would hand the app `sync://` while its station AppImage runs
    // a real worker off the same file.
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "async_worker": "true""#,
    );

    let error = parse_err(&contents);

    assert!(matches!(
        error,
        ManifestError::WrongType {
            field: "async_worker",
            expected: "a boolean",
            found: "a string",
            ..
        }
    ));
    let message = error.to_string();
    assert!(
        message.contains("/apps/tfsapp-test/tfsapp.config.json"),
        "{message}"
    );
}

// --- workers, and async_worker's desugaring into it ------------------------

#[test]
fn workers_is_parsed_in_declared_order() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "workers": [
            { "transports": ["courant", "planifie", "fond"] },
            { "transports": ["scheduler_default"] }
        ]"#,
    );

    let manifest = parse_ok(&contents).manifest;

    assert_eq!(manifest.workers.len(), 2);
    assert_eq!(
        manifest.workers[0].transports,
        ["courant", "planifie", "fond"]
    );
    // Absent `count` is the default of one.
    assert_eq!(manifest.workers[0].count, 1);
    assert_eq!(manifest.workers[1].transports, ["scheduler_default"]);
}

#[test]
fn a_declared_count_is_read() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "workers": [
            { "transports": ["fond"], "count": 3 }
        ]"#,
    );

    assert_eq!(parse_ok(&contents).manifest.workers[0].count, 3);
}

#[test]
fn async_worker_true_desugars_to_one_declaration_consuming_async() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "async_worker": true"#,
    );

    let manifest = parse_ok(&contents).manifest;

    assert!(manifest.async_worker);
    assert_eq!(manifest.workers.len(), 1);
    assert_eq!(manifest.workers[0].transports, ["async"]);
    assert_eq!(manifest.workers[0].count, 1);
}

#[test]
fn async_worker_absent_desugars_to_no_workers() {
    assert!(parse_ok(MINIMAL).manifest.workers.is_empty());
}

#[test]
fn spelling_both_async_worker_and_workers_is_refused() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "async_worker": false, "workers": [{"transports": ["a"]}]"#,
    );

    let error = parse_err(&contents);

    assert!(matches!(
        error,
        ManifestError::ConflictingWorkerDeclaration { .. }
    ));
    let message = error.to_string();
    assert!(message.contains("\"async_worker\""), "{message}");
    assert!(message.contains("\"workers\""), "{message}");
}

#[test]
fn workers_must_be_an_array() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "workers": {"transports": ["a"]}"#,
    );

    let error = parse_err(&contents);

    assert!(matches!(error, ManifestError::WorkersInvalid { .. }));
    let message = error.to_string();
    assert!(message.contains("must be an array"), "{message}");
    assert!(message.contains("found an object"), "{message}");
}

#[test]
fn a_worker_declaration_must_be_an_object() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "workers": ["not-an-object"]"#,
    );

    let error = parse_err(&contents);

    let message = error.to_string();
    assert!(message.contains("workers[0]"), "{message}");
    assert!(message.contains("must be an object"), "{message}");
}

#[test]
fn transports_must_be_a_non_empty_array() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "workers": [{"transports": []}]"#,
    );

    let error = parse_err(&contents);

    let message = error.to_string();
    assert!(message.contains("workers[0].transports"), "{message}");
    assert!(message.contains("non-empty array"), "{message}");
}

#[test]
fn a_transport_name_must_be_a_non_empty_string() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "workers": [{"transports": ["  "]}]"#,
    );

    let error = parse_err(&contents);

    let message = error.to_string();
    assert!(message.contains("workers[0].transports[0]"), "{message}");
    assert!(message.contains("non-empty string"), "{message}");
}

#[test]
fn count_must_be_a_positive_integer() {
    for bad_count in ["0", "-1", "\"3\"", "2.5"] {
        let contents = MINIMAL.replace(
            r#""app_version": "0.6.0""#,
            &format!(
                r#""app_version": "0.6.0", "workers": [{{"transports": ["a"], "count": {bad_count}}}]"#
            ),
        );

        let error = parse_err(&contents);

        let message = error.to_string();
        assert!(
            message.contains("workers[0].count"),
            "count {bad_count}: {message}"
        );
        assert!(
            message.contains("positive integer"),
            "count {bad_count}: {message}"
        );
    }
}

#[test]
fn a_transport_repeated_across_declarations_is_refused() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "workers": [
            { "transports": ["courant"] },
            { "transports": ["courant"] }
        ]"#,
    );

    let error = parse_err(&contents);

    assert!(matches!(
        error,
        ManifestError::DuplicateWorkerTransport { ref transport, .. } if transport == "courant"
    ));
    assert!(error.to_string().contains("\"courant\""), "{error}");
}

#[test]
fn a_transport_repeated_within_one_declaration_is_refused() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "workers": [
            { "transports": ["courant", "courant"] }
        ]"#,
    );

    assert!(matches!(
        parse_err(&contents),
        ManifestError::DuplicateWorkerTransport { .. }
    ));
}

#[test]
fn a_count_above_the_cap_falls_back_to_the_cap_and_says_so() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "workers": [{"transports": ["fond"], "count": 9}]"#,
    );

    let loaded = parse_ok(&contents);

    assert_eq!(loaded.manifest.workers[0].count, super::WORKER_COUNT_CAP);
    assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
    assert!(
        loaded.warnings[0].contains("workers[0].count"),
        "{}",
        loaded.warnings[0]
    );
    assert!(loaded.warnings[0].contains('9'), "{}", loaded.warnings[0]);
}

#[test]
fn a_scheduler_transport_asked_for_more_than_once_falls_back_to_one_and_says_so() {
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "workers": [{"transports": ["scheduler_default"], "count": 2}]"#,
    );

    let loaded = parse_ok(&contents);

    assert_eq!(loaded.manifest.workers[0].count, 1);
    assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
    assert!(
        loaded.warnings[0].contains("scheduler"),
        "{}",
        loaded.warnings[0]
    );
}

#[test]
fn both_fallbacks_can_fire_together_and_each_is_reported() {
    // A scheduler transport asked for above the cap: the cap fallback fires
    // first (9 -> 4), then the scheduler fallback fires on the post-cap
    // count (4 -> 1) — two warnings, not a silent clamp straight to one.
    let contents = MINIMAL.replace(
        r#""app_version": "0.6.0""#,
        r#""app_version": "0.6.0", "workers": [{"transports": ["scheduler_default"], "count": 9}]"#,
    );

    let loaded = parse_ok(&contents);

    assert_eq!(loaded.manifest.workers[0].count, 1);
    assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
}

// --- reading from disk ----------------------------------------------------

#[test]
fn load_reads_the_manifest_from_a_project_directory() {
    let project = tempfile::tempdir().expect("a temp project");
    std::fs::write(project.path().join(MANIFEST_FILE), MINIMAL).expect("the manifest is written");

    let loaded = load(project.path()).expect("it loads");

    assert_eq!(loaded.manifest.identifier, "dev.local.tfsapp-test");
}

#[test]
fn a_project_without_a_manifest_says_which_file_is_missing() {
    let project = tempfile::tempdir().expect("a temp project");

    let error = load(project.path()).expect_err("there is no manifest");

    assert!(matches!(error, ManifestError::Unreadable { .. }));
    assert!(error.to_string().contains(MANIFEST_FILE), "{error}");
}

// --- the identity it feeds ------------------------------------------------

#[test]
fn the_identity_reads_the_window_title_from_product_name() {
    let manifest = parse_ok(FULL).manifest;

    let identity = manifest.identity(Path::new("/apps/tfsapp-test"));

    assert_eq!(identity.identifier, "dev.local.tfsapp-test");
    // One field, one surface: the generated `.desktop` `Name=` will read this
    // same value, which is what stops the two from drifting apart.
    assert_eq!(identity.product_name, "TFS App Test");
    assert_eq!(
        identity.icon_path,
        Some(PathBuf::from("/apps/tfsapp-test/assets/icon.png"))
    );
}

#[test]
fn an_app_with_no_icon_declares_none_rather_than_a_guess() {
    let manifest = parse_ok(MINIMAL).manifest;

    let identity = manifest.identity(Path::new("/apps/tfsapp-test"));

    assert_eq!(identity.icon_path, None);
}
