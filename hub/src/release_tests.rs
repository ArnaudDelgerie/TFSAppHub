use std::{collections::HashMap, fs, path::Path};

use super::{
    download_to, fetch_latest_release_at, fetch_text, parse_sha256sums, resolve_assets,
    sha256_file, verify, GitHubAsset, GitHubRelease, ReleaseError, VerifyOutcome, RELEASES_REPO,
};

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
        body: None,
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
    // No "body" key at all in this response — defaults to `None` rather than
    // failing to deserialise.
    assert_eq!(release.body, None);
    let assets = resolve_assets(&release).expect("both assets present");
    assert_eq!(assets.archive_name, "demo-1.2.0.tar.gz");
    assert_eq!(assets.checksums_url, "https://example.test/SHA256SUMS.txt");
}

#[test]
fn a_release_tag_must_name_a_canonical_version() {
    let error = resolve_assets(&release(
        "release-1.2.0",
        vec![asset("demo-1.2.0.tar.gz"), asset("SHA256SUMS.txt")],
    ))
    .expect_err("a tag without v is not a release version");

    assert!(matches!(error, ReleaseError::InvalidTag { .. }), "{error}");
    assert!(error.to_string().contains("release-1.2.0"), "{error}");
    assert!(error.to_string().contains("v<app_version>"), "{error}");
}

#[test]
fn a_source_archive_must_agree_with_its_release_tag() {
    let error = resolve_assets(&release(
        "v1.2.0",
        vec![asset("demo-1.3.0.tar.gz"), asset("SHA256SUMS.txt")],
    ))
    .expect_err("an archive for another version is refused");

    assert!(
        matches!(error, ReleaseError::ArchiveTagMismatch { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("demo-1.3.0.tar.gz"), "{error}");
    assert!(error.to_string().contains("-1.2.0.tar.gz"), "{error}");
}

#[test]
fn a_release_s_body_is_read_when_present_and_none_when_explicitly_null() {
    let (base_url, handle) = stub_once(
        200,
        "application/json",
        r#"{"tag_name": "v1.0.0", "html_url": "https://x", "assets": [], "body": "Fixes stuff."}"#,
    );
    let release = fetch_latest_release_at(&base_url, "example/demo").expect("a good release");
    handle.join().expect("the stub thread finishes");
    assert_eq!(release.body, Some("Fixes stuff.".to_string()));

    let (base_url, handle) = stub_once(
        200,
        "application/json",
        r#"{"tag_name": "v1.0.0", "html_url": "https://x", "assets": [], "body": null}"#,
    );
    let release = fetch_latest_release_at(&base_url, "example/demo").expect("a good release");
    handle.join().expect("the stub thread finishes");
    assert_eq!(release.body, None);
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

#[test]
fn download_to_streams_the_response_body_to_disk() {
    let (base_url, handle) = stub_once(200, "application/octet-stream", "hello archive");
    let temp = tempfile::tempdir().expect("a temp dir");
    let target = temp.path().join("archive.bin");

    download_to(&format!("{base_url}/asset"), &target).expect("a successful download");
    handle.join().expect("the stub thread finishes");

    assert_eq!(
        fs::read_to_string(&target).expect("the downloaded file"),
        "hello archive"
    );
}

#[test]
fn fetch_text_returns_the_response_body_as_a_string() {
    let (base_url, handle) = stub_once(200, "text/plain", "deadbeef  demo.tar.gz\n");

    let body = fetch_text(&format!("{base_url}/SHA256SUMS.txt")).expect("a successful fetch");
    handle.join().expect("the stub thread finishes");

    assert_eq!(body, "deadbeef  demo.tar.gz\n");
}

#[test]
fn sha256_file_hashes_a_files_bytes_streaming_through_the_hasher() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let path = temp.path().join("content");
    fs::write(&path, b"hello world").expect("a written fixture");

    let digest = sha256_file(&path).expect("a computed hash");
    assert_eq!(
        digest,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

#[test]
fn sha256_file_reports_a_missing_file_rather_than_panicking() {
    let error = sha256_file(Path::new("/nonexistent/path/to/nothing")).expect_err("no such file");
    assert!(matches!(error, ReleaseError::Io(_)));
}

#[test]
fn parse_sha256sums_reads_hash_and_name_pairs_case_insensitively() {
    let body = "DEADBEEF  demo-1.0.0.tar.gz\n\ncafef00d *SHA256SUMS.txt\n";

    let parsed = parse_sha256sums(body);

    assert_eq!(
        parsed.get("demo-1.0.0.tar.gz").map(String::as_str),
        Some("deadbeef")
    );
    assert_eq!(
        parsed.get("SHA256SUMS.txt").map(String::as_str),
        Some("cafef00d")
    );
}

#[test]
fn verify_matches_a_hash_case_insensitively() {
    let mut expected = HashMap::new();
    expected.insert("demo.tar.gz".to_string(), "deadbeef".to_string());

    assert_eq!(
        verify(&expected, "demo.tar.gz", "DEADBEEF"),
        VerifyOutcome::Match
    );
}

#[test]
fn verify_reports_a_mismatch_for_a_tampered_download() {
    let mut expected = HashMap::new();
    expected.insert("demo.tar.gz".to_string(), "deadbeef".to_string());

    assert_eq!(
        verify(&expected, "demo.tar.gz", "00000000"),
        VerifyOutcome::Mismatch {
            expected: "deadbeef".to_string(),
            actual: "00000000".to_string(),
        }
    );
}

#[test]
fn verify_reports_a_missing_entry_rather_than_passing_by_absence() {
    let expected = HashMap::new();

    assert_eq!(
        verify(&expected, "demo.tar.gz", "deadbeef"),
        VerifyOutcome::MissingEntry
    );
}

#[test]
fn releases_repo_is_non_empty_and_parses_as_owner_slash_repo() {
    let mut parts = RELEASES_REPO.split('/');
    let owner = parts.next().filter(|part| !part.is_empty());
    let repo = parts.next().filter(|part| !part.is_empty());

    assert!(
        owner.is_some() && repo.is_some() && parts.next().is_none(),
        "RELEASES_REPO ({RELEASES_REPO:?}) must parse as exactly one owner/repo pair"
    );
}
