use super::{check, HubUpdateCheck};
use crate::release::fetch_latest_release_at;

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
