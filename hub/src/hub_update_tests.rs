use std::fs;

use super::{
    anchor_state, check, resolve_appimage_target, update_at, update_at_after_anchor,
    HubUpdateCheck, HubUpdateError, MissingAnchorHalf, UpdateOutcome,
};
use crate::{
    hub_bin,
    paths::Paths,
    release::{fetch_latest_release_at, RELEASES_REPO},
};

/// Start a `tiny_http` server that answers exactly one request with a JSON
/// `releases/latest` body, then stops — mirrors `release_tests.rs`'s own
/// `stub_once`, kept local rather than shared across two test modules for one
/// helper each.
fn stub_release(body: String) -> (String, std::thread::JoinHandle<()>) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("a local stub server");
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(address) => address.port(),
        other => panic!("unexpected listen address: {other:?}"),
    };
    let handle = std::thread::spawn(move || {
        let request = server.recv().expect("one request");
        let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
            .expect("a valid header");
        let response = tiny_http::Response::from_string(body)
            .with_status_code(200)
            .with_header(header);
        request.respond(response).expect("a sent response");
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

fn release_body(tag: &str, assets: &str) -> String {
    format!(
        r#"{{
            "tag_name": "{tag}",
            "html_url": "https://github.com/example/hub/releases/tag/{tag}",
            "assets": [{assets}]
        }}"#
    )
}

fn asset_json(name: &str) -> String {
    format!(
        r#"{{"name": "{name}", "browser_download_url": "https://example.test/{name}", "size": 1024}}"#
    )
}

fn fetch(body: String) -> crate::release::GitHubRelease {
    let (base_url, handle) = stub_release(body);
    let release = fetch_latest_release_at(&base_url, "example/hub").expect("a fetched release");
    handle.join().expect("the stub thread to finish cleanly");
    release
}

#[test]
fn a_newer_tag_with_both_assets_is_available() {
    let assets = format!(
        "{}, {}",
        asset_json("TFSAppHub_0.2.0_amd64.AppImage"),
        asset_json("SHA256SUMS.txt")
    );
    let release = fetch(release_body("v0.2.0", &assets));
    let current = semver::Version::parse("0.1.0").unwrap();

    match check(&current, &release) {
        HubUpdateCheck::Available {
            version,
            asset_name,
            asset_url,
            checksums_url,
        } => {
            assert_eq!(version, semver::Version::parse("0.2.0").unwrap());
            assert_eq!(asset_name, "TFSAppHub_0.2.0_amd64.AppImage");
            assert!(asset_url.ends_with("TFSAppHub_0.2.0_amd64.AppImage"));
            assert!(checksums_url.ends_with("SHA256SUMS.txt"));
        }
        other => panic!("expected Available, got {other:?}"),
    }
}

#[test]
fn an_equal_tag_is_up_to_date() {
    let assets = format!(
        "{}, {}",
        asset_json("TFSAppHub_0.1.0_amd64.AppImage"),
        asset_json("SHA256SUMS.txt")
    );
    let release = fetch(release_body("v0.1.0", &assets));
    let current = semver::Version::parse("0.1.0").unwrap();

    assert_eq!(check(&current, &release), HubUpdateCheck::UpToDate);
}

#[test]
fn an_older_tag_on_the_forge_is_up_to_date_never_a_downgrade() {
    let assets = format!(
        "{}, {}",
        asset_json("TFSAppHub_0.1.0_amd64.AppImage"),
        asset_json("SHA256SUMS.txt")
    );
    let release = fetch(release_body("v0.1.0", &assets));
    let current = semver::Version::parse("0.2.0").unwrap();

    assert_eq!(check(&current, &release), HubUpdateCheck::UpToDate);
}

#[test]
fn a_newer_tag_with_no_appimage_is_unavailable() {
    let assets = asset_json("SHA256SUMS.txt");
    let release = fetch(release_body("v0.2.0", &assets));
    let current = semver::Version::parse("0.1.0").unwrap();

    match check(&current, &release) {
        HubUpdateCheck::Unavailable(reason) => assert!(reason.contains("AppImage")),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn a_newer_tag_with_two_appimages_is_unavailable_as_ambiguous() {
    let assets = format!(
        "{}, {}, {}",
        asset_json("TFSAppHub_0.2.0_amd64.AppImage"),
        asset_json("TFSAppHub_0.2.0_arm64.AppImage"),
        asset_json("SHA256SUMS.txt")
    );
    let release = fetch(release_body("v0.2.0", &assets));
    let current = semver::Version::parse("0.1.0").unwrap();

    match check(&current, &release) {
        HubUpdateCheck::Unavailable(reason) => assert!(reason.contains("more than one")),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn a_newer_tag_with_no_sums_file_is_unavailable() {
    let assets = asset_json("TFSAppHub_0.2.0_amd64.AppImage");
    let release = fetch(release_body("v0.2.0", &assets));
    let current = semver::Version::parse("0.1.0").unwrap();

    match check(&current, &release) {
        HubUpdateCheck::Unavailable(reason) => assert!(reason.contains("SHA256SUMS.txt")),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn a_tag_that_is_not_a_version_is_unavailable() {
    let assets = format!(
        "{}, {}",
        asset_json("TFSAppHub_latest_amd64.AppImage"),
        asset_json("SHA256SUMS.txt")
    );
    let release = fetch(release_body("not-a-version", &assets));
    let current = semver::Version::parse("0.1.0").unwrap();

    match check(&current, &release) {
        HubUpdateCheck::Unavailable(reason) => assert!(reason.contains("not-a-version")),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn resolve_appimage_target_is_none_when_unset() {
    assert_eq!(resolve_appimage_target(None), None);
}

#[test]
fn resolve_appimage_target_is_none_when_empty() {
    assert_eq!(resolve_appimage_target(Some("")), None);
}

#[test]
fn resolve_appimage_target_is_the_path_when_set() {
    assert_eq!(
        resolve_appimage_target(Some("/home/user/Downloads/tfsapp-hub.AppImage")),
        Some("/home/user/Downloads/tfsapp-hub.AppImage".into())
    );
}

#[test]
fn anchor_state_is_ready_when_both_halves_are_present() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let binary = dir.path().join("tfsapp-hub.previous");
    let registry = dir.path().join("registry.json.previous");
    fs::write(&binary, b"hub v1").expect("the previous binary");
    fs::write(&registry, b"{}").expect("the registry snapshot");

    assert_eq!(anchor_state(&binary, &registry), Ok(()));
}

#[test]
fn anchor_state_names_both_halves_missing_when_neither_exists() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let binary = dir.path().join("tfsapp-hub.previous");
    let registry = dir.path().join("registry.json.previous");

    assert_eq!(
        anchor_state(&binary, &registry),
        Err(MissingAnchorHalf::Both)
    );
}

#[test]
fn anchor_state_names_the_binary_missing_when_only_the_registry_snapshot_exists() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let binary = dir.path().join("tfsapp-hub.previous");
    let registry = dir.path().join("registry.json.previous");
    fs::write(&registry, b"{}").expect("the registry snapshot");

    assert_eq!(
        anchor_state(&binary, &registry),
        Err(MissingAnchorHalf::Binary)
    );
}

#[test]
fn anchor_state_names_the_registry_missing_when_only_the_binary_exists() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let binary = dir.path().join("tfsapp-hub.previous");
    let registry = dir.path().join("registry.json.previous");
    fs::write(&binary, b"hub v1").expect("the previous binary");

    assert_eq!(
        anchor_state(&binary, &registry),
        Err(MissingAnchorHalf::Registry)
    );
}

#[test]
fn anchor_state_treats_an_empty_binary_as_missing() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let binary = dir.path().join("tfsapp-hub.previous");
    let registry = dir.path().join("registry.json.previous");
    fs::write(&binary, b"").expect("an empty previous binary");
    fs::write(&registry, b"{}").expect("the registry snapshot");

    assert_eq!(
        anchor_state(&binary, &registry),
        Err(MissingAnchorHalf::Binary)
    );
}

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

/// A `tiny_http` server that answers the three requests one `--update` check
/// makes — `RELEASES_REPO`'s `releases/latest`, the `.AppImage` asset, and
/// `SHA256SUMS.txt` — in whatever order they arrive, then stops. Mirrors
/// `install_tests.rs`'s own `stub_release`, adapted to the hub's fixed
/// releases repo and a single `.AppImage` asset rather than a `.tar.gz`.
/// `sums_body`, not just the asset bytes, is a parameter so the
/// checksum-mismatch test can serve one that does not match.
fn stub_hub_release(
    tag: &str,
    asset_name: &str,
    asset_bytes: Vec<u8>,
    sums_body: String,
) -> (String, std::thread::JoinHandle<()>) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("a local stub server");
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(address) => address.port(),
        other => panic!("unexpected listen address: {other:?}"),
    };
    let base_url = format!("http://127.0.0.1:{port}");
    let release_path = format!("/repos/{RELEASES_REPO}/releases/latest");
    let asset_path = format!("/assets/{asset_name}");
    let release_body = format!(
        r#"{{"tag_name": "{tag}", "html_url": "{base_url}/releases/tag/{tag}", "assets": [
            {{"name": "{asset_name}", "browser_download_url": "{base_url}{asset_path}", "size": {asset_size}}},
            {{"name": "SHA256SUMS.txt", "browser_download_url": "{base_url}/assets/SHA256SUMS.txt", "size": {sums_size}}}
        ]}}"#,
        asset_size = asset_bytes.len(),
        sums_size = sums_body.len(),
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
            } else if url == asset_path {
                request
                    .respond(tiny_http::Response::from_data(asset_bytes.clone()))
                    .expect("respond with the asset");
            } else if url == "/assets/SHA256SUMS.txt" {
                request
                    .respond(tiny_http::Response::from_string(sums_body.clone()))
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

fn sha256sums_line(name: &str, bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}  {name}\n", hasher.finalize())
}

#[test]
fn the_whole_flow_swaps_both_files_snapshots_the_registry_and_anchors_the_old_binary() {
    let (_base, paths) = temp_paths();
    let downloads = tempfile::tempdir().expect("a downloads dir");
    let appimage_path = downloads.path().join("tfsapp-hub-0.1.0.AppImage");
    fs::write(&appimage_path, b"hub v1 bytes").expect("the running image fixture");

    let asset_name = "TFSAppHub_0.2.0_amd64.AppImage";
    let asset_bytes = b"hub v2 bytes".to_vec();
    let sums = sha256sums_line(asset_name, &asset_bytes);
    let (base_url, handle) = stub_hub_release("v0.2.0", asset_name, asset_bytes.clone(), sums);

    let scratch = tempfile::tempdir().expect("a scratch dir");
    let current = semver::Version::parse("0.1.0").unwrap();

    let outcome = update_at(
        &paths,
        scratch.path(),
        &base_url,
        Some(appimage_path.to_str().unwrap()),
        &current,
        true,
    )
    .expect("the update succeeds");
    handle.join().expect("the stub thread to finish cleanly");

    match outcome {
        UpdateOutcome::Updated {
            version,
            stable_path,
            appimage_path: reported_appimage_path,
        } => {
            assert_eq!(version, semver::Version::parse("0.2.0").unwrap());
            assert_eq!(stable_path, paths.hub_executable_path());
            assert_eq!(reported_appimage_path, appimage_path);
        }
        other => panic!("expected Updated, got {other:?}"),
    }

    // Both targets carry the new bytes.
    assert_eq!(
        fs::read(paths.hub_executable_path()).expect("the stable copy exists"),
        asset_bytes
    );
    assert_eq!(
        fs::read(&appimage_path).expect("$APPIMAGE was overwritten"),
        asset_bytes
    );

    // The anchor holds the old stable copy — refreshed from $APPIMAGE by step
    // 3 before being renamed aside by step 8, so it carries the old bytes.
    assert_eq!(
        fs::read(hub_bin::anchor_path(&paths)).expect("the anchor binary exists"),
        b"hub v1 bytes"
    );

    // The registry snapshot exists and is readable JSON — an empty registry,
    // since nothing was ever installed in this temp root.
    let snapshot = fs::read_to_string(hub_bin::anchor_registry_path(&paths))
        .expect("the registry snapshot exists");
    let snapshot: serde_json::Value =
        serde_json::from_str(&snapshot).expect("the snapshot is valid JSON");
    assert_eq!(snapshot["apps"], serde_json::json!([]));
}

#[test]
fn a_step_eight_install_failure_is_undone_in_place() {
    let (_base, paths) = temp_paths();
    let downloads = tempfile::tempdir().expect("a downloads dir");
    let appimage = downloads.path().join("hub.AppImage");
    fs::write(&appimage, b"hub v1").unwrap();
    let asset_name = "hub.AppImage";
    let bytes = b"hub v2".to_vec();
    let (base, handle) = stub_hub_release(
        "v0.2.0",
        asset_name,
        bytes,
        sha256sums_line(asset_name, b"hub v2"),
    );
    let scratch = tempfile::tempdir().unwrap();

    let error = update_at_after_anchor(
        &paths,
        scratch.path(),
        &base,
        Some(appimage.to_str().unwrap()),
        &semver::Version::parse("0.1.0").unwrap(),
        true,
        |_| {
            fs::remove_file(scratch.path().join(asset_name)).unwrap();
        },
    );
    handle.join().unwrap();
    assert!(matches!(
        error,
        Err(HubUpdateError::StableSwapUndone { .. })
    ));
    assert_eq!(fs::read(paths.hub_executable_path()).unwrap(), b"hub v1");
    assert!(!hub_bin::anchor_path(&paths).exists());
    assert!(!hub_bin::anchor_registry_path(&paths).exists());
}

#[test]
fn a_step_eight_failure_that_cannot_be_undone_names_the_broken_launchers() {
    let (_base, paths) = temp_paths();
    let downloads = tempfile::tempdir().expect("a downloads dir");
    let appimage = downloads.path().join("hub.AppImage");
    fs::write(&appimage, b"hub v1").unwrap();
    let asset_name = "hub.AppImage";
    let bytes = b"hub v2".to_vec();
    let (base, handle) = stub_hub_release(
        "v0.2.0",
        asset_name,
        bytes,
        sha256sums_line(asset_name, b"hub v2"),
    );
    let scratch = tempfile::tempdir().unwrap();

    let error = update_at_after_anchor(
        &paths,
        scratch.path(),
        &base,
        Some(appimage.to_str().unwrap()),
        &semver::Version::parse("0.1.0").unwrap(),
        true,
        |paths| {
            fs::create_dir(paths.hub_executable_path()).unwrap();
        },
    )
    .expect_err("the directory blocks both the install and the undo");
    handle.join().unwrap();
    let message = error.to_string();
    assert!(message.contains("is missing"));
    assert!(message.contains("Every generated launcher is down"));
    assert!(message.contains("--rollback"));
    assert!(hub_bin::anchor_path(&paths).is_file());
    assert!(hub_bin::anchor_registry_path(&paths).is_file());
}

#[test]
fn appimage_already_the_stable_copy_gets_one_swap_not_two() {
    let (_base, paths) = temp_paths();
    // $APPIMAGE names the stable copy itself — the `.desktop`-launched case
    // (the plan's "Two files, not one").
    let stable_path = paths.hub_executable_path();
    fs::create_dir_all(stable_path.parent().unwrap()).expect("bin/ exists");
    fs::write(&stable_path, b"hub v1 bytes").expect("the running image fixture");

    let asset_name = "TFSAppHub_0.2.0_amd64.AppImage";
    let asset_bytes = b"hub v2 bytes".to_vec();
    let sums = sha256sums_line(asset_name, &asset_bytes);
    let (base_url, handle) = stub_hub_release("v0.2.0", asset_name, asset_bytes.clone(), sums);

    let scratch = tempfile::tempdir().expect("a scratch dir");
    let current = semver::Version::parse("0.1.0").unwrap();

    let outcome = update_at(
        &paths,
        scratch.path(),
        &base_url,
        Some(stable_path.to_str().unwrap()),
        &current,
        true,
    )
    .expect("the update succeeds");
    handle.join().expect("the stub thread to finish cleanly");

    assert!(matches!(outcome, UpdateOutcome::Updated { .. }));
    assert_eq!(
        fs::read(&stable_path).expect("the stable copy exists"),
        asset_bytes
    );
}

#[test]
fn a_checksum_mismatch_moves_nothing() {
    let (_base, paths) = temp_paths();
    let downloads = tempfile::tempdir().expect("a downloads dir");
    let appimage_path = downloads.path().join("tfsapp-hub-0.1.0.AppImage");
    fs::write(&appimage_path, b"hub v1 bytes").expect("the running image fixture");

    let asset_name = "TFSAppHub_0.2.0_amd64.AppImage";
    let asset_bytes = b"hub v2 bytes".to_vec();
    // A checksum line for the right name but the wrong content.
    let bad_sums = sha256sums_line(asset_name, b"not the real bytes");
    let (base_url, handle) = stub_hub_release("v0.2.0", asset_name, asset_bytes.clone(), bad_sums);

    let scratch = tempfile::tempdir().expect("a scratch dir");
    let current = semver::Version::parse("0.1.0").unwrap();

    let error = update_at(
        &paths,
        scratch.path(),
        &base_url,
        Some(appimage_path.to_str().unwrap()),
        &current,
        true,
    )
    .expect_err("a checksum mismatch refuses");
    handle.join().expect("the stub thread to finish cleanly");

    assert!(matches!(error, HubUpdateError::ChecksumMismatch { .. }));

    // Nothing moved: no anchor, no registry snapshot, $APPIMAGE untouched.
    assert!(!hub_bin::anchor_path(&paths).is_file());
    assert!(!hub_bin::anchor_registry_path(&paths).is_file());
    assert_eq!(
        fs::read(&appimage_path).expect("$APPIMAGE is untouched"),
        b"hub v1 bytes"
    );
    // Step 3's refresh is idempotent maintenance, not part of "the change":
    // the stable copy exists and matches the running image, never the
    // rejected asset.
    assert_eq!(
        fs::read(paths.hub_executable_path()).expect("the stable copy was refreshed"),
        b"hub v1 bytes"
    );
}
