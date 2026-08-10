use super::{fetch_latest_release_at, resolve_assets, GitHubAsset, GitHubRelease, ReleaseError};

/// Start a `tiny_http` server that answers exactly one request with `status`
/// and `body`, then stops — enough to stand in for one GitHub API call
/// without a real network. Returns the base URL to point [`fetch_latest_release_at`]
/// at.
fn stub_once(
    status: u16,
    content_type: &'static str,
    body: &'static str,
) -> (String, std::thread::JoinHandle<()>) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("a local stub server");
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(address) => address.port(),
        other => panic!("unexpected listen address: {other:?}"),
    };
    let handle = std::thread::spawn(move || {
        let request = server.recv().expect("one request");
        let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
            .expect("a valid header");
        let response = tiny_http::Response::from_string(body)
            .with_status_code(status)
            .with_header(header);
        request.respond(response).expect("a sent response");
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

fn release(tag: &str, assets: Vec<GitHubAsset>) -> GitHubRelease {
    GitHubRelease {
        tag_name: tag.to_string(),
        html_url: format!("https://github.com/example/demo/releases/tag/{tag}"),
        assets,
    }
}

fn asset(name: &str) -> GitHubAsset {
    GitHubAsset {
        name: name.to_string(),
        browser_download_url: format!("https://example.test/{name}"),
        size: 1024,
    }
}

#[test]
fn a_good_release_resolves_its_tag_and_assets() {
    let body = r#"{
        "tag_name": "v1.2.0",
        "html_url": "https://github.com/example/demo/releases/tag/v1.2.0",
        "assets": [
            {"name": "demo-1.2.0.tar.gz", "browser_download_url": "https://example.test/demo-1.2.0.tar.gz", "size": 4096},
            {"name": "SHA256SUMS.txt", "browser_download_url": "https://example.test/SHA256SUMS.txt", "size": 128}
        ]
    }"#;
    let (base_url, handle) = stub_once(200, "application/json", body);

    let release = fetch_latest_release_at(&base_url, "example/demo").expect("a good release");
    handle.join().expect("the stub thread finishes");

    assert_eq!(release.tag_name, "v1.2.0");
    let assets = resolve_assets(&release).expect("both assets present");
    assert_eq!(assets.archive_name, "demo-1.2.0.tar.gz");
    assert_eq!(assets.checksums_url, "https://example.test/SHA256SUMS.txt");
}

#[test]
fn a_release_with_no_tar_gz_asset_is_refused_by_name() {
    let error = resolve_assets(&release("v1.0.0", vec![asset("SHA256SUMS.txt")]))
        .expect_err("no archive to install");
    assert!(
        matches!(error, ReleaseError::MissingAsset { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("v1.0.0"), "{error}");
}

#[test]
fn a_release_with_two_tar_gz_assets_is_refused_as_ambiguous() {
    let error = resolve_assets(&release(
        "v1.0.0",
        vec![
            asset("demo-1.0.0.tar.gz"),
            asset("demo-1.0.0-extra.tar.gz"),
            asset("SHA256SUMS.txt"),
        ],
    ))
    .expect_err("ambiguous archive");
    assert!(matches!(error, ReleaseError::MissingAsset { .. }));
}

#[test]
fn a_release_with_no_checksums_file_is_refused_naming_it() {
    let error = resolve_assets(&release("v1.0.0", vec![asset("demo-1.0.0.tar.gz")]))
        .expect_err("no checksums to verify against");
    assert!(matches!(
        error,
        ReleaseError::MissingAsset {
            missing: "SHA256SUMS.txt",
            ..
        }
    ));
}

#[test]
fn a_missing_repo_or_release_is_not_found() {
    let (base_url, handle) = stub_once(404, "application/json", "{}");

    let error = fetch_latest_release_at(&base_url, "example/nothing-here").expect_err("a 404");
    handle.join().expect("the stub thread finishes");

    assert!(matches!(error, ReleaseError::NotFound));
}

#[test]
fn a_403_with_a_rate_limit_body_is_reported_as_rate_limited() {
    let body = r#"{"message": "API rate limit exceeded"}"#;
    let (base_url, handle) = stub_once(403, "application/json", body);

    let error = fetch_latest_release_at(&base_url, "example/demo").expect_err("a 403");
    handle.join().expect("the stub thread finishes");

    assert!(matches!(error, ReleaseError::RateLimited));
    assert!(
        error.to_string().contains("wait"),
        "the message should say what to do: {error}"
    );
}

#[test]
fn a_malformed_json_payload_is_an_invalid_response() {
    let (base_url, handle) = stub_once(200, "application/json", "{ not json");

    let error = fetch_latest_release_at(&base_url, "example/demo").expect_err("bad JSON");
    handle.join().expect("the stub thread finishes");

    assert!(matches!(error, ReleaseError::InvalidResponse(_)));
}
