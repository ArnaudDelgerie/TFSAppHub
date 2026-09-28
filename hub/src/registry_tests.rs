use super::{
    load, now_timestamp, restore_for_rollback, update, Platform, ReferenceKind, Registry,
    RegistryEntry, RegistryError, RollbackRestoreOutcome, Source, SourceKind, State,
};
use crate::paths::Paths;

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

fn platform() -> Platform {
    Platform {
        php_version: "8.5".to_string(),
        extensions_hash: "a1b2c3d4".repeat(8),
    }
}

fn entry(id: &str) -> RegistryEntry {
    RegistryEntry {
        id: id.to_string(),
        identifier: format!("dev.local.{id}"),
        source: Source {
            kind: SourceKind::LocalArchive,
            location: format!("/home/arnaud/Dev/{id}-0.1.0.tar.gz"),
            reference: None,
            reference_kind: None,
            index: None,
        },
        app_version: "0.6.0".to_string(),
        source_revision: "sha256:deadbeef".to_string(),
        app_port: Some(8123),
        platform: platform(),
        state: State::Ready,
        installed_at: now_timestamp(),
        updated_at: now_timestamp(),
        unknown: serde_json::Map::new(),
    }
}

#[test]
fn a_registry_survives_a_write_and_a_read() {
    let (_base, paths) = temp_paths();
    let mut registry = Registry::default();
    registry.stamp("0.1.0", platform());
    registry.upsert(entry("tfsapp-test"));
    registry.upsert(RegistryEntry {
        source: Source {
            kind: SourceKind::Release,
            location: "example/app".to_string(),
            reference: Some("v1.4.0".to_string()),
            reference_kind: Some(ReferenceKind::Tag),
            index: Some("github".to_string()),
        },
        app_port: None,
        state: State::NeedsRevalidation,
        ..entry("other")
    });

    update(&paths, |stored| *stored = registry.clone()).expect("it writes");

    assert_eq!(load(&paths).expect("it reads"), registry);
}

#[test]
fn a_missing_file_is_an_empty_registry_not_an_error() {
    // The state of every machine that has never installed anything, which is
    // the one a fresh hub meets first.
    let (_base, paths) = temp_paths();

    let registry = load(&paths).expect("nothing installed is not a failure");

    assert_eq!(registry, Registry::default());
    assert!(registry.apps.is_empty());
    assert_eq!(registry.hub_version, None);
}

#[test]
fn an_unreadable_registry_says_so_and_names_the_file() {
    let (_base, paths) = temp_paths();
    std::fs::create_dir_all(paths.hub_root()).expect("the hub root");
    std::fs::write(paths.registry_path(), "{ not json").expect("a corrupted file");

    let error = load(&paths).expect_err("it must not be read as empty");

    assert!(matches!(error, RegistryError::Malformed { .. }));
    let message = error.to_string();
    assert!(message.contains("registry.json"), "{message}");
    // Saying nothing was changed matters: the natural next thought on reading
    // "not a readable registry" is "did it just wipe my installs?".
    assert!(message.contains("Nothing was changed"), "{message}");
}

#[test]
fn a_registry_holding_a_local_path_entry_is_refused_as_a_whole() {
    // Plan 066 deleted the `local-path` kind with no migration: no user
    // existed yet (decision 009's reason 4). What is left to pin is that an
    // old registry is refused loudly rather than read as something strange.
    let (_base, paths) = temp_paths();
    std::fs::create_dir_all(paths.hub_root()).expect("the hub root");
    std::fs::write(
        paths.registry_path(),
        r#"{
  "hub_version": "0.2.0",
  "apps": [
    {
      "id": "tfsapp-test",
      "identifier": "dev.local.tfsapp-test",
      "source": { "kind": "local-path", "location": "/home/arnaud/Dev/TFSAppTest" },
      "app_version": "0.6.0",
      "source_revision": "sha256:deadbeef",
      "platform": { "php_version": "8.5", "extensions_hash": "abc" },
      "state": "ready",
      "installed_at": "2026-08-07T10:00:00Z",
      "updated_at": "2026-08-07T10:00:00Z"
    }
  ]
}"#,
    )
    .expect("a written registry");

    let error = load(&paths).expect_err("a local-path entry is not a source kind");

    assert!(matches!(error, RegistryError::Malformed { .. }), "{error}");
    let message = error.to_string();
    assert!(message.contains("local-path"), "{message}");
    assert!(message.contains("Nothing was changed"), "{message}");
}

/// Save `registry` and read the file back as raw JSON — the shape a *later*
/// hub version will meet, which is what these tests are really about.
fn saved_json(paths: &Paths, registry: &Registry) -> (String, serde_json::Value) {
    update(paths, |stored| *stored = registry.clone()).expect("it writes");
    let raw = std::fs::read_to_string(paths.registry_path()).expect("it is there");
    let json = serde_json::from_str(&raw).expect("valid JSON");
    (raw, json)
}

#[test]
fn the_file_on_disk_uses_the_names_the_design_settled_on() {
    // The JSON is the interface between two hub versions, so the spelling of
    // its keys and enum values is worth pinning, not just the round trip.
    let (_base, paths) = temp_paths();
    let mut registry = Registry::default();
    registry.stamp("0.1.0", platform());
    registry.upsert(RegistryEntry {
        state: State::NeedsRevalidation,
        source: Source {
            kind: SourceKind::Release,
            location: "example/app".to_string(),
            reference: Some("v1.4.0".to_string()),
            reference_kind: Some(ReferenceKind::Tag),
            index: Some("github".to_string()),
        },
        ..entry("tfsapp-test")
    });

    let (raw, json) = saved_json(&paths, &registry);

    let app = &json["apps"][0];
    assert_eq!(app["source"]["kind"].as_str(), Some("release"));
    // `ref` in the file, `reference` in Rust, where `ref` is a keyword.
    assert_eq!(app["source"]["ref"].as_str(), Some("v1.4.0"));
    assert_eq!(app["source"]["reference_kind"].as_str(), Some("tag"));
    assert_eq!(app["source"]["index"].as_str(), Some("github"));
    assert_eq!(app["state"].as_str(), Some("needs-revalidation"));
    assert_eq!(json["hub_version"].as_str(), Some("0.1.0"));
    assert_eq!(json["platform"]["php_version"].as_str(), Some("8.5"));
    // Written for a human to open in an editor, and ending in a newline like
    // every other text file on the machine.
    assert!(raw.contains("\n  \"apps\""), "{raw}");
    assert!(raw.ends_with("}\n"), "{raw}");
}

#[test]
fn a_local_archive_writes_its_kind_and_no_ref() {
    // The one source kind with no selector at all: a local release archive
    // has a location and nothing to pin.
    let (_base, paths) = temp_paths();
    let mut registry = Registry::default();
    registry.upsert(entry("tfsapp-test"));

    let (_raw, json) = saved_json(&paths, &registry);

    assert_eq!(
        json["apps"][0]["source"]["kind"].as_str(),
        Some("local-archive")
    );
    assert!(json["apps"][0]["source"].get("ref").is_none());
    assert!(json["apps"][0]["source"].get("reference_kind").is_none());
    assert!(json["apps"][0]["source"].get("index").is_none());
}

#[test]
fn an_app_with_no_pinned_port_writes_no_port_key() {
    let (_base, paths) = temp_paths();
    let mut registry = Registry::default();
    registry.upsert(RegistryEntry {
        app_port: None,
        ..entry("tfsapp-test")
    });

    let (_raw, json) = saved_json(&paths, &registry);

    assert!(json["apps"][0].get("app_port").is_none());
    assert_eq!(load(&paths).expect("it reads").apps[0].app_port, None);
}

// --- forward compatibility -------------------------------------------------

#[test]
fn fields_a_newer_hub_wrote_survive_a_read_modify_write() {
    // A user who tries a newer hub and steps back must not silently lose what
    // it recorded — at the top level and inside the entry it wrote.
    let (_base, paths) = temp_paths();
    std::fs::create_dir_all(paths.hub_root()).expect("the hub root");
    std::fs::write(
        paths.registry_path(),
        r#"{
  "hub_version": "9.9.9",
  "telemetry_opt_in": false,
  "apps": [
    {
      "id": "tfsapp-test",
      "identifier": "dev.local.tfsapp-test",
      "source": { "kind": "local-archive", "location": "/releases/demo-0.1.0.tar.gz" },
      "app_version": "0.6.0",
      "source_revision": "sha256:deadbeef",
      "platform": { "php_version": "8.5", "extensions_hash": "abc" },
      "state": "ready",
      "installed_at": "2026-08-07T10:00:00Z",
      "updated_at": "2026-08-07T10:00:00Z",
      "sandbox_profile": "strict"
    }
  ]
}"#,
    )
    .expect("a registry from the future");

    update(&paths, |registry| {
        registry.stamp("0.1.0", platform());
        registry.upsert(entry("second"));
    })
    .expect("the rewrite succeeds");

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(paths.registry_path()).expect("it is there"))
            .expect("valid JSON");

    assert_eq!(json["telemetry_opt_in"].as_bool(), Some(false));
    assert_eq!(json["apps"][0]["sandbox_profile"].as_str(), Some("strict"));
    // What this hub did understand was still applied.
    assert_eq!(json["hub_version"].as_str(), Some("0.1.0"));
    assert_eq!(json["apps"][1]["id"].as_str(), Some("second"));
}

#[test]
fn upsert_keeps_an_unknown_field_the_replaced_entry_carried() {
    // The same rule one level down, and the reason `upsert` merges rather than
    // overwrites: `update <id>` replaces an entry wholesale, and a field a
    // newer hub had recorded for that app must not go with it.
    let mut registry = Registry::default();
    let mut existing = entry("tfsapp-test");
    existing
        .unknown
        .insert("sandbox_profile".to_string(), "strict".into());
    registry.upsert(existing);

    registry.upsert(RegistryEntry {
        app_version: "0.7.0".to_string(),
        ..entry("tfsapp-test")
    });

    assert_eq!(registry.apps.len(), 1);
    let stored = registry.get("tfsapp-test").expect("still one entry");
    assert_eq!(stored.app_version, "0.7.0");
    assert_eq!(
        stored.unknown.get("sandbox_profile"),
        Some(&serde_json::Value::from("strict"))
    );
}

// --- the collection helpers ------------------------------------------------

#[test]
fn upsert_replaces_in_place_and_keeps_install_order() {
    let mut registry = Registry::default();
    registry.upsert(entry("first"));
    registry.upsert(entry("second"));

    registry.upsert(RegistryEntry {
        app_version: "9.0.0".to_string(),
        ..entry("first")
    });

    let ids: Vec<&str> = registry.apps.iter().map(|app| app.id.as_str()).collect();
    assert_eq!(ids, ["first", "second"]);
    assert_eq!(
        registry.get("first").expect("it is there").app_version,
        "9.0.0"
    );
}

#[test]
fn remove_returns_the_entry_it_dropped() {
    let mut registry = Registry::default();
    registry.upsert(entry("first"));
    registry.upsert(entry("second"));

    let removed = registry.remove("first").expect("it was there");

    assert_eq!(removed.id, "first");
    assert!(registry.get("first").is_none());
    assert_eq!(registry.apps.len(), 1);
    assert!(registry.remove("first").is_none());
}

// --- concurrency -----------------------------------------------------------

#[test]
fn concurrent_installs_all_land_in_one_valid_file() {
    // Installs can genuinely overlap, and the failure this guards against is
    // not a corrupt file but a lost one: without the lock held across the whole
    // read-modify-write, every writer would start from the same empty registry
    // and the last rename would be the only app installed.
    let (_base, paths) = temp_paths();
    let writers = 8;

    std::thread::scope(|scope| {
        for index in 0..writers {
            let paths = &paths;
            scope.spawn(move || {
                update(paths, |registry| {
                    registry.upsert(entry(&format!("app-{index}")));
                })
                .expect("every writer succeeds");
            });
        }
    });

    let registry = load(&paths).expect("the file is readable");
    let mut ids: Vec<String> = registry.apps.iter().map(|app| app.id.clone()).collect();
    ids.sort();
    let expected: Vec<String> = (0..writers).map(|index| format!("app-{index}")).collect();
    assert_eq!(ids, expected);
}

#[test]
fn a_write_never_leaves_its_temp_file_behind() {
    let (_base, paths) = temp_paths();

    update(&paths, |stored| *stored = Registry::default()).expect("it writes");

    let leftovers: Vec<_> = std::fs::read_dir(paths.hub_root())
        .expect("the hub root")
        .filter_map(Result::ok)
        .map(|found| found.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

// --- rollback restore ------------------------------------------------------

#[test]
fn rollback_restore_copies_the_anchor_bytes_when_app_entries_match() {
    let (_base, paths) = temp_paths();
    let snapshot_path = paths.hub_root().join("registry.json.previous");
    let anchor = r#"{
  "hub_version": "0.1.0",
  "apps": []
}
"#;
    std::fs::create_dir_all(paths.hub_root()).expect("the hub root");
    std::fs::write(&snapshot_path, anchor).expect("the anchor");
    update(&paths, |registry| {
        registry.hub_version = Some("0.2.0".into());
    })
    .expect("the live registry");

    let outcome = restore_for_rollback(&paths, &snapshot_path).expect("the restore succeeds");

    assert_eq!(outcome, RollbackRestoreOutcome::RestoredBytes);
    assert_eq!(
        std::fs::read(paths.registry_path()).expect("the restored registry"),
        anchor.as_bytes()
    );
}

#[test]
fn rollback_restore_merges_the_locked_live_entries_and_their_unknown_fields() {
    let (_base, paths) = temp_paths();
    let snapshot_path = paths.hub_root().join("registry.json.previous");
    std::fs::create_dir_all(paths.hub_root()).expect("the hub root");
    std::fs::write(
        &snapshot_path,
        r#"{
  "hub_version": "0.1.0",
  "platform": { "php_version": "8.4", "extensions_hash": "old" },
  "apps": [],
  "snapshot_only": true
}
"#,
    )
    .expect("the anchor");
    std::fs::write(
        paths.registry_path(),
        r#"{
  "hub_version": "0.2.0",
  "platform": { "php_version": "8.5", "extensions_hash": "new" },
  "apps": [{
    "id": "kept",
    "identifier": "dev.local.kept",
    "source": { "kind": "local-archive", "location": "/releases/demo-0.1.0.tar.gz" },
    "app_version": "2.0.0",
    "source_revision": "revision",
    "platform": { "php_version": "8.5", "extensions_hash": "new" },
    "state": "ready",
    "installed_at": "2026-01-01T00:00:00Z",
    "updated_at": "2026-01-01T00:00:00Z",
    "entry_from_newer_hub": "kept"
  }],
  "top_level_from_newer_hub": { "enabled": true }
}
"#,
    )
    .expect("the live registry");

    let outcome = restore_for_rollback(&paths, &snapshot_path).expect("the merge succeeds");

    let RollbackRestoreOutcome::Merged { live } = outcome else {
        panic!("divergent app entries must merge");
    };
    assert_eq!(live.get("kept").unwrap().app_version, "2.0.0");
    assert_eq!(
        live.get("kept")
            .unwrap()
            .unknown
            .get("entry_from_newer_hub"),
        Some(&serde_json::Value::from("kept"))
    );
    assert_eq!(
        live.unknown.get("top_level_from_newer_hub"),
        Some(&serde_json::json!({ "enabled": true }))
    );

    let restored = load(&paths).expect("the merged registry reads");
    assert_eq!(restored.hub_version.as_deref(), Some("0.1.0"));
    assert_eq!(restored.platform.as_ref().unwrap().php_version, "8.4");
    assert_eq!(
        restored.unknown.get("top_level_from_newer_hub"),
        Some(&serde_json::json!({ "enabled": true }))
    );
    assert_eq!(
        restored
            .get("kept")
            .unwrap()
            .unknown
            .get("entry_from_newer_hub"),
        Some(&serde_json::Value::from("kept"))
    );
}

// --- timestamps ------------------------------------------------------------

#[test]
fn timestamps_are_rfc_3339_in_utc() {
    // Checked by shape rather than by re-parsing: the `time` crate is pulled
    // in for formatting only, and adding its parser to the binary to assert
    // one test would be a real dependency for an imagined need.
    let stamp = now_timestamp();

    assert!(stamp.ends_with('Z'), "{stamp}");
    let (date, rest) = stamp.split_once('T').unwrap_or_else(|| panic!("{stamp}"));
    assert_eq!(date.len(), 10, "{stamp}");
    assert_eq!(date.matches('-').count(), 2, "{stamp}");
    assert_eq!(rest.matches(':').count(), 2, "{stamp}");
    // Sortable as text, which is the only reason to keep it a string at all.
    assert!(stamp.as_str() > "2026-01-01T00:00:00Z", "{stamp}");
}

#[test]
fn the_platform_prints_short_enough_to_read() {
    assert_eq!(platform().to_string(), "8.5+a1b2c3d4");
}

#[test]
fn local_archive_source_round_trips_without_a_selector() {
    let (_base, paths) = temp_paths();
    let mut registry = Registry::default();
    registry.upsert(RegistryEntry {
        source: Source {
            kind: SourceKind::LocalArchive,
            location: "/mnt/release/demo-1.2.0.tar.gz".to_string(),
            reference: None,
            reference_kind: None,
            index: None,
        },
        ..entry("demo")
    });
    let (raw, json) = saved_json(&paths, &registry);
    assert_eq!(json["apps"][0]["source"]["kind"], "local-archive");
    assert!(!raw.contains("reference_kind"));
    assert_eq!(load(&paths).unwrap(), registry);
}
