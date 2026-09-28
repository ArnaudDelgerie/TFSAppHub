use super::{reconcile, ReconcileError};
use crate::{
    paths::Paths,
    platform,
    registry::{self, now_timestamp, Platform, Registry, RegistryEntry, Source, SourceKind, State},
};

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

/// A fingerprint that cannot be a real probe's answer — no interpreter prints
/// a made-up version like this — which is what makes it reliably "different"
/// without needing the bundled FrankenPHP the real one would take.
fn implausible_platform() -> Platform {
    Platform {
        php_version: "0.0".to_string(),
        extensions_hash: "not-a-real-probe".to_string(),
    }
}

fn entry(id: &str, platform: Platform, state: State) -> RegistryEntry {
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
        app_port: None,
        platform,
        state,
        installed_at: now_timestamp(),
        updated_at: now_timestamp(),
        unknown: serde_json::Map::new(),
    }
}

/// The bundled interpreter, or a reason to skip — the same gate
/// `install_tests.rs` uses, duplicated rather than imported: each `_tests.rs`
/// file is self-contained, and a 170 MB download is not something a unit test
/// should trigger. Only the tests that need `reconcile` to actually probe
/// (a genuinely different `hub_version`) take this gate; the equal-version
/// case never reaches the probe at all.
fn resources_present() -> bool {
    let candidates = platform::bundled_frankenphp();
    let present = candidates.iter().any(|path| path.is_file());
    if !present {
        eprintln!(
            "skipped: none of {candidates:?} are there — run `make resources` to cover this one"
        );
    }
    present
}

#[test]
fn a_matching_hub_version_probes_nothing_and_changes_nothing() {
    let (_base, paths) = temp_paths();
    let mut registry = Registry::default();
    registry.stamp("0.6.0", implausible_platform());
    // A fingerprint the real probe could never produce: if `reconcile` probed
    // here despite the matching version, this entry would be marked, which is
    // exactly what this test is checking does not happen.
    registry.upsert(entry("demo", implausible_platform(), State::Ready));
    registry::update(&paths, |stored| *stored = registry.clone()).expect("a seeded registry");

    reconcile(&paths, "0.6.0").expect("a matching version is not an error");

    let after = registry::load(&paths).expect("it reads");
    assert_eq!(after, registry);
}

#[test]
fn a_different_version_with_a_matching_fingerprint_is_restamped_and_touches_no_app() {
    if !resources_present() {
        return;
    }
    let (_base, paths) = temp_paths();
    let running = platform::hub_platform().expect("a probe");

    let mut registry = Registry::default();
    registry.stamp("0.1.0", running.clone());
    registry.upsert(entry("demo", running.clone(), State::Ready));
    registry::update(&paths, |stored| *stored = registry.clone()).expect("a seeded registry");

    reconcile(&paths, "0.2.0").expect("it reconciles");

    let after = registry::load(&paths).expect("it reads");
    assert_eq!(after.hub_version.as_deref(), Some("0.2.0"));
    assert_eq!(after.platform, Some(running));
    assert_eq!(after.get("demo").expect("still there").state, State::Ready);
}

#[test]
fn a_different_fingerprint_marks_only_the_apps_that_differ() {
    if !resources_present() {
        return;
    }
    let (_base, paths) = temp_paths();
    let running = platform::hub_platform().expect("a probe");

    let mut registry = Registry::default();
    registry.stamp("0.1.0", implausible_platform());
    registry.upsert(entry("matches", running.clone(), State::Ready));
    registry.upsert(entry("stale", implausible_platform(), State::Ready));
    registry::update(&paths, |stored| *stored = registry.clone()).expect("a seeded registry");

    reconcile(&paths, "0.2.0").expect("it reconciles");

    let after = registry::load(&paths).expect("it reads");
    assert_eq!(
        after.get("matches").expect("still there").state,
        State::Ready
    );
    assert_eq!(
        after.get("stale").expect("still there").state,
        State::NeedsRevalidation
    );
    assert_eq!(after.platform, Some(running));
}

#[test]
fn an_already_broken_app_is_left_alone() {
    if !resources_present() {
        return;
    }
    let (_base, paths) = temp_paths();
    let running = platform::hub_platform().expect("a probe");

    let mut registry = Registry::default();
    registry.stamp("0.1.0", implausible_platform());
    registry.upsert(entry("broken", implausible_platform(), State::Broken));
    registry::update(&paths, |stored| *stored = registry.clone()).expect("a seeded registry");

    reconcile(&paths, "0.2.0").expect("it reconciles");

    let after = registry::load(&paths).expect("it reads");
    assert_eq!(
        after.get("broken").expect("still there").state,
        State::Broken
    );
    assert_eq!(after.platform, Some(running));
}

#[test]
fn a_registry_error_is_the_reconcile_error_it_wraps() {
    // `From<RegistryError>` is exercised for real by every test above through
    // `?`; this one just checks the `Display` wiring rather than duplicating
    // `registry_tests.rs`'s own coverage of what makes a registry unreadable.
    let error = ReconcileError::Registry(registry::RegistryError::Unserialisable {
        detail: "boom".to_string(),
    });
    assert!(error.to_string().contains("boom"));
}
