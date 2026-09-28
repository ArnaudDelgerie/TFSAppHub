use std::{fs, path::Path};

use super::{absolute_paths, child_args, launch_header, prepare_hub_log, resolve, OpenError};
use crate::{
    launch::Source as LaunchSource,
    lifecycle,
    paths::Paths,
    registry::{self, now_timestamp, Platform, Registry, RegistryEntry, Source, SourceKind, State},
};

/// A registry holding one app, written to disk under `paths` — `open` reads the
/// real file, so its tests write one rather than hand it a struct.
fn install(paths: &Paths, entry: RegistryEntry) {
    registry::update(paths, |stored| {
        *stored = Registry {
            apps: vec![entry],
            ..Registry::default()
        };
    })
    .expect("a written registry");
}

fn entry(id: &str, state: State) -> RegistryEntry {
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
        platform: Platform {
            php_version: "8.5".to_string(),
            extensions_hash: "a1b2c3d4".repeat(8),
        },
        state,
        installed_at: now_timestamp(),
        updated_at: now_timestamp(),
        unknown: serde_json::Map::new(),
    }
}

/// The installed snapshot as `open` meets it: a manifest, and nothing else —
/// `open`'s parent half validates identity, never the tree's completeness, which
/// the installer already did.
fn snapshot(app_dir: &Path, identifier: &str, icon_path: Option<&str>) {
    fs::create_dir_all(app_dir).expect("an app dir");
    let icon_line = match icon_path {
        Some(path) => format!(",\n              \"icon_path\": \"{path}\""),
        None => String::new(),
    };
    fs::write(
        app_dir.join("tfsapp.config.json"),
        format!(
            r#"{{
              "product_name": "Demo App",
              "identifier": "{identifier}",
              "project_name": "demo",
              "app_version": "0.6.0"{icon_line}
            }}"#
        ),
    )
    .expect("a manifest");
}

/// [`snapshot`], plus a `composer.json` whose `require` is spliced in —
/// `revalidate::revalidate` needs one to run `composer install` against.
fn snapshot_with_composer(app_dir: &Path, identifier: &str, composer_require: &str) {
    snapshot(app_dir, identifier, None);
    fs::write(
        app_dir.join("composer.json"),
        format!("{{\"require\": {{{composer_require}}}}}"),
    )
    .expect("a composer.json");
}

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

/// A ready app, registered and snapshotted, under `id`.
fn ready(paths: &Paths, id: &str) {
    install(paths, entry(id, State::Ready));
    snapshot(
        &paths.app_dir(id).expect("an app dir path"),
        &format!("dev.local.{id}"),
        None,
    );
}

#[test]
fn an_installed_app_resolves_to_its_snapshot_identity_and_data_dir() {
    let (_base, paths) = temp_paths();
    ready(&paths, "demo");

    let resolved = resolve(&paths, "demo").expect("a resolvable app");

    assert_eq!(
        resolved.source,
        LaunchSource::Installed {
            id: "demo".to_string()
        }
    );
    assert_eq!(resolved.label, "demo");
    assert_eq!(resolved.identity.identifier, "dev.local.demo");
    // From the manifest, not from the registry: one field feeds the window title
    // and the `.desktop` Name= alike, with no bake in between.
    assert_eq!(resolved.identity.product_name, "Demo App");
    assert_eq!(resolved.app_dir, paths.app_dir("demo").expect("an app dir"));
    // The app's data dir hangs off `identifier` alone — the same path a packaged
    // AppImage of this app resolves, which is what makes the two the same app.
    assert_eq!(
        resolved.state_root,
        paths
            .app_data_dir("dev.local.demo")
            .expect("a data dir path")
    );
    // Named, never created: resolving is not opening.
    assert!(!resolved.state_root.exists());
}

#[test]
fn an_icon_is_resolved_inside_the_installed_snapshot() {
    let (_base, paths) = temp_paths();
    install(&paths, entry("demo", State::Ready));
    let app_dir = paths.app_dir("demo").expect("an app dir path");
    snapshot(&app_dir, "dev.local.demo", Some("assets/icon.png"));

    let resolved = resolve(&paths, "demo").expect("a resolvable app");

    // Inside the snapshot, because that is the tree that will still be there
    // when the window opens — the source it was copied from may not be.
    assert_eq!(
        resolved.identity.icon_path.as_deref(),
        Some(app_dir.join("assets/icon.png").as_path())
    );
}

#[test]
fn an_unknown_id_names_the_two_commands_that_answer_it() {
    let (_base, paths) = temp_paths();
    ready(&paths, "demo");

    let error = resolve(&paths, "typo").expect_err("no such app");

    assert!(matches!(error, OpenError::NotInstalled { .. }));
    let message = error.to_string();
    assert!(message.contains("typo"), "names the app: {message}");
    assert!(
        message.contains("tfsapp-hub list"),
        "names how to look: {message}"
    );
    assert!(
        message.contains("tfsapp-hub install"),
        "names how to fix it: {message}"
    );
}

#[test]
fn a_broken_app_is_refused_rather_than_opened_to_fail_deeper() {
    let (_base, paths) = temp_paths();
    install(&paths, entry("demo", State::Broken));
    snapshot(
        &paths.app_dir("demo").expect("an app dir path"),
        "dev.local.demo",
        None,
    );

    let error = resolve(&paths, "demo").expect_err("a broken app");

    assert!(matches!(error, OpenError::Broken { .. }));
    let message = error.to_string();
    assert!(message.contains("demo"), "names the app: {message}");
    // Both ways out, because which one is right depends on whether the user
    // wants the newer app or the older hub.
    assert!(
        message.contains("tfsapp-hub update demo"),
        "names the app-side fix: {message}"
    );
    assert!(
        message.contains("--rollback"),
        "names the hub-side fix: {message}"
    );
}

#[test]
fn a_needs_revalidation_app_resolves_without_running_composer() {
    let (_base, paths) = temp_paths();
    install(&paths, entry("demo", State::NeedsRevalidation));
    let app_dir = paths.app_dir("demo").expect("an app dir path");
    snapshot_with_composer(&app_dir, "dev.local.demo", "");

    let resolved = resolve(&paths, "demo").expect("the pending launch resolves");
    assert_eq!(
        resolved.pending_revalidation,
        Some(entry("demo", State::NeedsRevalidation).platform)
    );

    let after = registry::load(&paths).expect("it reads");
    let after_entry = after.get("demo").expect("still there");
    assert_eq!(after_entry.state, State::NeedsRevalidation);
}

#[test]
fn a_needs_revalidation_app_with_an_unsatisfiable_lock_still_resolves() {
    let (_base, paths) = temp_paths();
    install(&paths, entry("demo", State::NeedsRevalidation));
    let app_dir = paths.app_dir("demo").expect("an app dir path");
    // No PHP will ever be 99.x — Composer refuses on the platform check
    // alone, with no package resolution or network involved.
    snapshot_with_composer(&app_dir, "dev.local.demo", "\"php\": \"^99.0\"");

    let resolved = resolve(&paths, "demo").expect("the pending launch resolves");
    assert!(resolved.pending_revalidation.is_some());
    let after = registry::load(&paths).expect("it reads");
    assert_eq!(
        after.get("demo").expect("still there").state,
        State::NeedsRevalidation
    );
}

#[test]
fn a_registered_app_whose_snapshot_is_gone_says_so() {
    let (_base, paths) = temp_paths();
    install(&paths, entry("demo", State::Ready));

    let error = resolve(&paths, "demo").expect_err("no snapshot");

    assert!(matches!(error, OpenError::NoSnapshot { .. }));
    assert!(
        error.to_string().contains("tfsapp-hub remove demo"),
        "names the way to drop the dangling entry: {error}"
    );
}

#[test]
fn a_snapshot_that_changed_identifier_is_refused_not_guessed() {
    let (_base, paths) = temp_paths();
    install(&paths, entry("demo", State::Ready));
    snapshot(
        &paths.app_dir("demo").expect("an app dir path"),
        "dev.local.something-else",
        None,
    );

    let error = resolve(&paths, "demo").expect_err("a changed identifier");

    assert!(matches!(error, OpenError::IdentityChanged { .. }));
    let message = error.to_string();
    // Both values, because the whole point is that the hub will not pick one:
    // the identifier names the data dir, so guessing would open the app onto
    // someone else's database.
    assert!(
        message.contains("dev.local.demo"),
        "names the registered one: {message}"
    );
    assert!(
        message.contains("dev.local.something-else"),
        "names the declared one: {message}"
    );
}

#[test]
fn the_child_carries_its_identity_in_argv() {
    let (_base, paths) = temp_paths();
    install(&paths, entry("demo", State::Ready));
    let app_dir = paths.app_dir("demo").expect("an app dir path");
    snapshot(&app_dir, "dev.local.demo", Some("icon.png"));

    let args = child_args(&resolve(&paths, "demo").expect("a resolvable app"), &[]);

    // Identity before I/O: the child has to apply all of this before anything
    // touches GTK, so everything it needs at that point travels in argv.
    assert_eq!(
        args,
        vec![
            "__open".to_string(),
            "--id".to_string(),
            "demo".to_string(),
            "--identity".to_string(),
            "dev.local.demo".to_string(),
            "--name".to_string(),
            "Demo App".to_string(),
            "--icon".to_string(),
            app_dir.join("icon.png").display().to_string(),
        ]
    );
}

#[test]
fn an_app_with_no_icon_passes_no_icon_argument() {
    let (_base, paths) = temp_paths();
    ready(&paths, "demo");

    let args = child_args(&resolve(&paths, "demo").expect("a resolvable app"), &[]);

    // Rather than an empty `--icon ""`, which the child would have to tell apart
    // from a path it failed to read.
    assert!(!args.iter().any(|argument| argument == "--icon"));
}

// --- launch_header -----------------------------------------------------

#[test]
fn launch_header_names_the_timestamp_command_app_and_pid() {
    let header = launch_header("2026-08-09T12:00:00Z", "open", "demo", 4242);

    assert_eq!(header, "=== 2026-08-09T12:00:00Z open demo (pid 4242) ===");
}

// --- prepare_hub_log -----------------------------------------------------

#[test]
fn prepare_hub_log_creates_the_dir_and_points_stdio_at_hub_log() {
    let (_base, paths) = temp_paths();
    let mut command = std::process::Command::new("sh");
    command.args(["-c", "printf 'child-line\\n'"]);

    let hub_log = prepare_hub_log(&paths, "dev.local.demo", &mut command)
        .expect("hub.log opened against a writable temp root");

    assert_eq!(
        hub_log,
        paths
            .app_data_dir("dev.local.demo")
            .expect("a data dir path")
            .join("log")
            .join("hub.log")
    );
    let status = command.status().expect("the child ran");
    assert!(status.success());
    assert_eq!(fs::read_to_string(&hub_log).unwrap(), "child-line\n");
}

#[test]
fn prepare_hub_log_rotates_an_oversized_file_before_opening_it() {
    let (_base, paths) = temp_paths();
    let data_dir = paths
        .create_app_data_dir("dev.local.demo")
        .expect("a created data dir");
    let log_dir = data_dir.join("log");
    fs::create_dir_all(&log_dir).expect("a created log dir");
    let hub_log = log_dir.join("hub.log");
    fs::write(
        &hub_log,
        vec![b'x'; tfsapp_core::log::MAX_LOG_BYTES as usize],
    )
    .unwrap();

    let mut command = std::process::Command::new("sh");
    command.args(["-c", "printf 'fresh-line\\n'"]);
    prepare_hub_log(&paths, "dev.local.demo", &mut command)
        .expect("hub.log opened against a writable temp root");
    command.status().expect("the child ran");

    assert!(log_dir.join("hub.log.1").exists());
    assert_eq!(fs::read_to_string(&hub_log).unwrap(), "fresh-line\n");
}

#[test]
fn prepare_hub_log_leaves_a_live_instance_s_log_untouched() {
    let (_base, paths) = temp_paths();
    let data_dir = paths
        .create_app_data_dir("dev.local.demo")
        .expect("a created data dir");
    let log_dir = data_dir.join("log");
    fs::create_dir_all(&log_dir).expect("a created log dir");
    let hub_log = log_dir.join("hub.log");
    let original = vec![b'x'; tfsapp_core::log::MAX_LOG_BYTES as usize];
    fs::write(&hub_log, &original).unwrap();
    let _serving = tfsapp_core::process::try_lock_file(&lifecycle::serving_lock_path(&data_dir))
        .expect("the serving lock opens")
        .expect("the serving lock is held for the live instance");

    let mut command = std::process::Command::new("sh");
    command.args(["-c", "printf 'hand-off-line\\n'"]);
    assert!(prepare_hub_log(&paths, "dev.local.demo", &mut command).is_none());

    assert!(!log_dir.join("hub.log.1").exists());
    assert_eq!(fs::read(&hub_log).unwrap(), original);
}

#[test]
fn prepare_hub_log_falls_back_to_inherited_stdio_when_the_dir_cannot_be_created() {
    use std::os::unix::fs::PermissionsExt;

    let (base, paths) = temp_paths();
    let mut perms = fs::metadata(base.path()).unwrap().permissions();
    perms.set_mode(0o500);
    fs::set_permissions(base.path(), perms.clone()).unwrap();

    let mut command = std::process::Command::new("sh");
    command.args(["-c", "true"]);
    let hub_log = prepare_hub_log(&paths, "dev.local.demo", &mut command);

    perms.set_mode(0o700);
    fs::set_permissions(base.path(), perms).unwrap();

    assert!(hub_log.is_none());
}

// --- the file batch -----------------------------------------------------

#[test]
fn the_child_argv_carries_the_batch_after_the_separator() {
    let (_base, paths) = temp_paths();
    ready(&paths, "demo");

    let spec = resolve(&paths, "demo").expect("a resolvable app");
    let args = child_args(
        &spec,
        &["/tmp/one file.md".to_string(), "--help".to_string()],
    );

    // Distinct arguments after a bare `--`: no quoting, no interpolation —
    // the separator is what says the rest are paths, and nothing else does.
    let separator = args.iter().position(|argument| argument == "--").unwrap();
    assert_eq!(
        args[separator + 1..],
        ["/tmp/one file.md".to_string(), "--help".to_string()]
    );
    // The separator exists only for a file-bearing launch.
    let bare = child_args(&spec, &[]);
    assert!(!bare.iter().any(|argument| argument == "--"));
}

#[test]
fn relative_paths_are_made_absolute_against_the_callers_working_directory() {
    let cwd = std::env::current_dir().expect("a working directory");
    let batch = absolute_paths(&["/abs/a.md".to_string(), "rel/b.md".to_string()])
        .expect("the caller's working directory is readable");

    // The absolute path is the caller's own spelling, untouched; the relative
    // one is read against the cwd this process was invoked from — made
    // absolute, never canonicalized, so what the request names is what the
    // caller named.
    assert_eq!(
        batch,
        vec![
            "/abs/a.md".to_string(),
            cwd.join("rel/b.md").display().to_string(),
        ]
    );
}
