use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{classify, resolve, tree_hash, Origin, SourceError};
use crate::registry::SourceKind;
use sha2::{Digest, Sha256};

/// None of this file's `resolve` calls reach the `Origin::Release` arm — every
/// case here is a local path or a git spelling — so a real base URL is never
/// dialed; this just has to be *some* string.
const UNUSED_BASE_URL: &str = "http://unused.invalid";

fn project(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("a source dir");
    fs::write(root.join("tfsapp.config.json"), "{}").expect("a manifest");
    fs::write(root.join("src/Kernel.php"), "<?php class Kernel {}").expect("a class");
}

fn release_archive(manifest: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    header.set_path("demo-1.2.0/").expect("a directory path");
    header.set_cksum();
    builder
        .append(&header, std::io::empty())
        .expect("the top-level directory");

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(manifest.len() as u64);
    header.set_mode(0o644);
    header
        .set_path("demo-1.2.0/tfsapp.config.json")
        .expect("a manifest path");
    header.set_cksum();
    builder
        .append(&header, manifest.as_bytes())
        .expect("the manifest");
    builder
        .into_inner()
        .expect("finish the tar")
        .finish()
        .expect("finish the gzip")
}

fn stub_release(archive: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
    let archive_name = "demo-1.2.0.tar.gz";
    let mut hasher = Sha256::new();
    hasher.update(&archive);
    let checksums = format!("{:x}  {archive_name}\n", hasher.finalize());
    let server = tiny_http::Server::http("127.0.0.1:0").expect("a local stub server");
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(address) => address.port(),
        other => panic!("unexpected listen address: {other:?}"),
    };
    let base_url = format!("http://127.0.0.1:{port}");
    let release = format!(
        r#"{{"tag_name":"v1.2.0","html_url":"{base_url}/release","assets":[{{"name":"{archive_name}","browser_download_url":"{base_url}/assets/{archive_name}","size":{}}},{{"name":"SHA256SUMS.txt","browser_download_url":"{base_url}/assets/SHA256SUMS.txt","size":{}}}]}}"#,
        archive.len(),
        checksums.len(),
    );
    let handle = std::thread::spawn(move || {
        for _ in 0..3 {
            let request = server.recv().expect("a request");
            match request.url() {
                "/repos/example/demo/releases/latest" => {
                    let header = tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"application/json"[..],
                    )
                    .expect("a header");
                    request
                        .respond(
                            tiny_http::Response::from_string(release.clone()).with_header(header),
                        )
                        .expect("release metadata");
                }
                "/assets/demo-1.2.0.tar.gz" => request
                    .respond(tiny_http::Response::from_data(archive.clone()))
                    .expect("archive"),
                "/assets/SHA256SUMS.txt" => request
                    .respond(tiny_http::Response::from_string(checksums.clone()))
                    .expect("checksums"),
                path => panic!("unexpected request path: {path}"),
            }
        }
    });
    (base_url, handle)
}

fn remote_manifest(version: &str, project_name: &str) -> String {
    format!(
        r#"{{"product_name":"Demo","identifier":"dev.local.demo","project_name":"{project_name}","app_version":"{version}"}}"#
    )
}

#[test]
fn a_release_manifest_must_confirm_its_tag_version() {
    let (base_url, handle) = stub_release(release_archive(&remote_manifest("1.3.0", "demo")));
    let scratch = tempfile::tempdir().expect("a scratch directory");

    let error = resolve(
        &classify("github:example/demo"),
        None,
        scratch.path(),
        &base_url,
    )
    .expect_err("a manifest version contradicting the tag is refused");
    handle.join().expect("the stub thread finishes");

    assert!(
        matches!(error, SourceError::ManifestVersionMismatch { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("v1.2.0"), "{error}");
    assert!(error.to_string().contains("1.3.0"), "{error}");
}

#[test]
fn a_release_manifest_refuses_pre_release_and_build_version_spellings() {
    for version in ["1.2.0-rc.1", "1.2.0+build.7"] {
        let (base_url, handle) = stub_release(release_archive(&remote_manifest(version, "demo")));
        let scratch = tempfile::tempdir().expect("a scratch directory");

        let error = resolve(
            &classify("github:example/demo"),
            None,
            scratch.path(),
            &base_url,
        )
        .expect_err("a suffixed manifest version is refused during resolution");
        handle.join().expect("the stub thread finishes");

        assert!(
            matches!(error, SourceError::ManifestVersionNotCanonical { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("MAJOR.MINOR.PATCH"), "{error}");
    }
}

#[test]
fn a_release_manifest_must_confirm_its_archive_project_name() {
    let (base_url, handle) = stub_release(release_archive(&remote_manifest("1.2.0", "other")));
    let scratch = tempfile::tempdir().expect("a scratch directory");

    let error = resolve(
        &classify("github:example/demo"),
        None,
        scratch.path(),
        &base_url,
    )
    .expect_err("a manifest project name contradicting the archive is refused");
    handle.join().expect("the stub thread finishes");

    assert!(
        matches!(error, SourceError::ManifestProjectNameMismatch { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("demo-1.2.0.tar.gz"), "{error}");
    assert!(error.to_string().contains("other"), "{error}");
}

#[test]
fn a_conforming_release_records_the_same_release_source() {
    let (base_url, handle) = stub_release(release_archive(&remote_manifest("1.2.0", "demo")));
    let scratch = tempfile::tempdir().expect("a scratch directory");

    let resolved = resolve(
        &classify("github:example/demo"),
        None,
        scratch.path(),
        &base_url,
    )
    .expect("a conforming release resolves");
    handle.join().expect("the stub thread finishes");

    assert_eq!(resolved.source.kind, SourceKind::Release);
    assert_eq!(resolved.source.location, "example/demo");
    assert_eq!(resolved.source.reference.as_deref(), Some("v1.2.0"));
    assert_eq!(resolved.source.index.as_deref(), Some("github"));
}

#[test]
fn the_same_tree_hashes_the_same_wherever_it_sits() {
    // What the value is for: a tree copied to another path is the same source,
    // and a hash that disagreed would report every install as changed.
    let one = tempfile::tempdir().expect("a temp dir");
    let other = tempfile::tempdir().expect("a second temp dir");
    project(one.path());
    project(other.path());

    assert_eq!(
        tree_hash(one.path()).expect("it hashes"),
        tree_hash(other.path()).expect("it hashes"),
    );
}

#[test]
fn an_edit_changes_the_hash() {
    let root = tempfile::tempdir().expect("a temp dir");
    project(root.path());
    let before = tree_hash(root.path()).expect("it hashes");

    fs::write(root.path().join("src/Kernel.php"), "<?php class Kernel { }")
        .expect("an edited class");

    assert_ne!(before, tree_hash(root.path()).expect("it hashes"));
}

#[test]
fn moving_content_to_another_name_changes_the_hash() {
    // The bytes are identical and the tree is not: without the path in the
    // hash, a rename would be invisible.
    let root = tempfile::tempdir().expect("a temp dir");
    project(root.path());
    let before = tree_hash(root.path()).expect("it hashes");

    fs::rename(
        root.path().join("src/Kernel.php"),
        root.path().join("src/AppKernel.php"),
    )
    .expect("a rename");

    assert_ne!(before, tree_hash(root.path()).expect("it hashes"));
}

#[test]
fn losing_the_executable_bit_changes_the_hash() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("a temp dir");
    project(root.path());
    let script = root.path().join("bin/console");
    fs::create_dir_all(root.path().join("bin")).expect("a bin dir");
    fs::write(&script, "#!/usr/bin/env php").expect("a script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("+x");
    let before = tree_hash(root.path()).expect("it hashes");

    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).expect("-x");

    assert_ne!(before, tree_hash(root.path()).expect("it hashes"));
}

#[test]
fn the_directories_that_churn_are_left_out() {
    // `var/cache` changes on every request the app serves. Hashing it would
    // report "changed since install" forever, which is the same as reporting
    // nothing at all.
    let root = tempfile::tempdir().expect("a temp dir");
    project(root.path());
    let before = tree_hash(root.path()).expect("it hashes");

    for churn in ["var/cache/dev", "vendor/symfony/console", "node_modules/x"] {
        fs::create_dir_all(root.path().join(churn)).expect("a churning dir");
        fs::write(root.path().join(churn).join("file"), "noise").expect("noise");
    }

    assert_eq!(before, tree_hash(root.path()).expect("it hashes"));

    // Only at the top level, though: a source file that happens to live under
    // a directory of that name still counts.
    fs::create_dir_all(root.path().join("src/var")).expect("a source subdir");
    fs::write(root.path().join("src/var/Holder.php"), "<?php").expect("a class");

    assert_ne!(before, tree_hash(root.path()).expect("it hashes"));
}

#[test]
fn a_string_is_classified_without_touching_the_disk() {
    // git-clone spellings — recognised only so `resolve` can refuse them
    // well, pointing at the release form instead of a mystifying "no such
    // directory".
    for spec in [
        "git@github.com:example/demo",
        "git@github.com:example/demo.git",
        "https://github.com/example/demo.git",
        "https://example.test/demo.git",
        "/does/not/exist.git",
    ] {
        assert_eq!(
            classify(spec),
            Origin::GitSpelling(spec.to_string()),
            "{spec}"
        );
    }

    // The canonical release form, and the https spelling it normalises to it.
    let canonical = Origin::Release {
        index: Some("github".to_string()),
        repo: "example/demo".to_string(),
    };
    assert_eq!(classify("github:example/demo"), canonical);
    assert_eq!(classify("https://github.com/example/demo"), canonical);
    assert_eq!(classify("https://github.com/example/demo/"), canonical);

    // Anything else — including a github.com URL carrying more than
    // owner/repo, and a non-GitHub URL this hub has no grammar row for — is
    // unrecognised, so `resolve` can refuse it naming the two kinds the hub
    // does install.
    for unrecognised in [
        "../TFSAppTest",
        "/home/arnaud/Dev/Demo",
        ".",
        "ssh://git@example.test/demo",
        "https://github.com/example/demo/tree/main",
    ] {
        assert_eq!(
            classify(unrecognised),
            Origin::Unrecognised(PathBuf::from(unrecognised)),
            "{unrecognised}"
        );
    }
}

#[test]
fn a_directory_is_refused_naming_the_publish_local_route() {
    // A working directory is not an install source (decision 009): the
    // refusal has to say what to do instead, or a developer with a tree in
    // hand is left guessing.
    let root = tempfile::tempdir().expect("a temp dir");
    project(root.path());
    let scratch = tempfile::tempdir().expect("a scratch dir");

    let error = resolve(
        &classify(&root.path().display().to_string()),
        None,
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .expect_err("a directory is not an install source");

    assert!(
        matches!(error, SourceError::DirectoryNotASource { .. }),
        "{error}"
    );
    let message = error.to_string();
    assert!(message.contains("is a directory"), "{message}");
    assert!(
        message.contains(&format!("tfsapp-hub publish {}", root.path().display())),
        "{message}"
    );
    assert!(
        message.contains(&format!("tfsapp-hub dev {}", root.path().display())),
        "{message}"
    );
}

#[test]
fn a_directory_named_like_an_archive_is_refused_the_same_way() {
    let root = tempfile::tempdir().expect("a temp dir");
    let shaped = root.path().join("demo-1.2.0.tar.gz");
    fs::create_dir_all(&shaped).expect("a directory named like an archive");
    let scratch = tempfile::tempdir().expect("a scratch dir");

    let error = resolve(
        &classify(&shaped.display().to_string()),
        None,
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .expect_err("a directory is not an install source, whatever its name");

    assert!(
        matches!(error, SourceError::DirectoryNotASource { .. }),
        "{error}"
    );
}

#[test]
fn a_source_that_cannot_be_installed_says_which_kind_of_problem_it_is() {
    let root = tempfile::tempdir().expect("a temp dir");
    project(root.path());
    let scratch = tempfile::tempdir().expect("a scratch dir");

    let error = resolve(
        &classify("/no/such/project"),
        None,
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .expect_err("no such source at all");
    assert!(
        matches!(error, SourceError::UnrecognisedSource { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("/no/such/project"), "{error}");
    assert!(error.to_string().contains("github:owner/repo"), "{error}");
    assert!(error.to_string().contains("SHA256SUMS.txt"), "{error}");

    let file = root.path().join("tfsapp.config.json");
    let error = resolve(
        &classify(&file.display().to_string()),
        None,
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .expect_err("a manifest file is not a source either");
    assert!(
        matches!(error, SourceError::UnrecognisedSource { .. }),
        "{error}"
    );

    // Recognised, and refused well: the message points at the release form
    // instead of an unexplained refusal.
    let error = resolve(
        &classify("https://example.test/demo.git"),
        None,
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .expect_err("a git clone URL is not a release spec");
    assert!(matches!(error, SourceError::GitSpelling { .. }), "{error}");
}

fn local_archive_fixture(manifest: &str) -> tempfile::TempDir {
    // Kept beside the shared `release_of` rather than folded into it: this
    // fixture's whole point is control over the manifest bytes and the
    // archive's name independently of each other, which a helper that reads
    // both from a project tree cannot express.
    let folder = tempfile::tempdir().unwrap();
    let mut blobs = std::collections::HashMap::new();
    blobs.insert("manifest".to_string(), manifest.as_bytes().to_vec());
    let entries = vec![crate::git::TreeEntry {
        path: PathBuf::from("tfsapp.config.json"),
        mode: 0o100644,
        object_id: "manifest".to_string(),
    }];
    crate::publish::build_archive(
        &entries,
        &mut crate::test_release::LocalBlobs(blobs),
        &[],
        "demo",
        "1.2.0",
        folder.path(),
    )
    .unwrap();
    folder
}

#[test]
fn local_archive_classifies_and_resolves_with_canonical_location() {
    let folder = local_archive_fixture(&remote_manifest("1.2.0", "demo"));
    let archive = folder.path().join("demo-1.2.0.tar.gz");
    assert_eq!(
        classify(&archive.display().to_string()),
        Origin::LocalArchive(archive.clone())
    );
    let scratch = tempfile::tempdir().unwrap();
    let resolved = resolve(
        &classify(&archive.display().to_string()),
        None,
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .unwrap();
    assert_eq!(resolved.source.kind, SourceKind::LocalArchive);
    assert_eq!(
        resolved.source.location,
        archive.canonicalize().unwrap().display().to_string()
    );
    assert_eq!(resolved.source.reference, None);
    assert_eq!(resolved.source.index, None);
    assert_eq!(resolved.revision, tree_hash(&resolved.root).unwrap());
}

#[test]
fn local_archive_refuses_missing_sums_mismatch_and_missing_entry() {
    let folder = local_archive_fixture(&remote_manifest("1.2.0", "demo"));
    let archive = folder.path().join("demo-1.2.0.tar.gz");
    let scratch = tempfile::tempdir().unwrap();
    let wrong_name = folder.path().join("renamed.tar.gz");
    fs::copy(&archive, &wrong_name).unwrap();
    let error = resolve(
        &Origin::LocalArchive(wrong_name),
        None,
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .unwrap_err();
    assert!(
        matches!(error, SourceError::ChecksumMissing { .. }),
        "{error}"
    );
    let mut bytes = fs::read(&archive).unwrap();
    bytes[20] ^= 1;
    fs::write(&archive, bytes).unwrap();
    let error = resolve(
        &Origin::LocalArchive(archive.clone()),
        None,
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .unwrap_err();
    assert!(
        matches!(error, SourceError::ChecksumMismatch { .. }),
        "{error}"
    );
    fs::remove_file(folder.path().join("SHA256SUMS.txt")).unwrap();
    let error = resolve(
        &Origin::LocalArchive(archive),
        None,
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .unwrap_err();
    assert!(
        matches!(error, SourceError::ChecksumFileMissing { .. }),
        "{error}"
    );
}

#[test]
fn a_local_archive_symlink_to_a_non_utf8_name_is_refused_not_a_panic() {
    use std::os::unix::ffi::OsStrExt;

    let folder = local_archive_fixture(&remote_manifest("1.2.0", "demo"));
    let target = folder
        .path()
        .join(std::ffi::OsStr::from_bytes(b"demo-\xff.tar.gz"));
    fs::rename(folder.path().join("demo-1.2.0.tar.gz"), &target).unwrap();
    let link = folder.path().join("link.tar.gz");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let error = resolve(
        &Origin::LocalArchive(link),
        None,
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .unwrap_err();
    assert!(matches!(error, SourceError::NotAFile { .. }), "{error}");
}

#[test]
fn local_archive_checks_manifest_name_version_and_ref() {
    let scratch = tempfile::tempdir().unwrap();
    for (version, name) in [("1.3.0", "demo"), ("1.2.0", "other")] {
        let folder = local_archive_fixture(&remote_manifest(version, name));
        let archive = folder.path().join("demo-1.2.0.tar.gz");
        let error = resolve(
            &Origin::LocalArchive(archive),
            None,
            scratch.path(),
            UNUSED_BASE_URL,
        )
        .unwrap_err();
        assert!(
            matches!(error, SourceError::ArchiveNameMismatch { .. }),
            "{error}"
        );
    }
    let folder = local_archive_fixture(&remote_manifest("1.2.0", "demo"));
    let archive = folder.path().join("demo-1.2.0.tar.gz");
    let error = resolve(
        &Origin::LocalArchive(archive),
        Some("v1.2.0"),
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .unwrap_err();
    assert!(
        matches!(error, SourceError::ReferenceOnLocalArchive { .. }),
        "{error}"
    );
}

#[test]
fn backup_is_identified_before_missing_checksums() {
    let folder = tempfile::tempdir().unwrap();
    let backup = folder.path().join("backup.tar.gz");
    let file = fs::File::create(&backup).unwrap();
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::fast(),
    ));
    let contents = br#"{"identifier":"dev.local.demo","app_version":"1.2.0","exported_at":"2026-01-01T00:00:00Z"}"#;
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "manifest.json", &contents[..])
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let error = resolve(
        &Origin::LocalArchive(backup),
        None,
        scratch.path(),
        UNUSED_BASE_URL,
    )
    .unwrap_err();
    assert!(matches!(error, SourceError::IsABackup { .. }), "{error}");
    assert!(error.to_string().contains("tfsapp-hub import <id>"));
}
