use super::{
    answer, answer_now, Context, UpdateCheckResult, REASON_LOCAL_SOURCE, REASON_NO_ANSWER_YET,
};
use crate::{paths::Paths, registry::Source, update_cache::CachedRelease};

fn local_source() -> Source {
    Source {
        kind: crate::registry::SourceKind::LocalArchive,
        location: "/home/arnaud/Dev/demo-0.1.0.tar.gz".to_string(),
        reference: None,
        reference_kind: None,
        index: None,
    }
}

fn release_source() -> Source {
    Source {
        kind: crate::registry::SourceKind::Release,
        location: "owner/repo".to_string(),
        reference: Some("v1.1.0".to_string()),
        reference_kind: Some(crate::registry::ReferenceKind::Tag),
        index: Some("github".to_string()),
    }
}

fn cached(tag: &str) -> CachedRelease {
    CachedRelease {
        checked_at: "2026-08-11T00:00:00Z".to_string(),
        tag: tag.to_string(),
        release_url: format!("https://github.com/owner/repo/releases/tag/{tag}"),
        notes: "release notes".to_string(),
        unknown: serde_json::Map::new(),
    }
}

#[test]
fn a_local_source_is_always_unavailable_local_source() {
    // Even with a cache entry sitting right there — the answer is `reason`,
    // not "no entry for this repo", because the question has no meaning for a
    // directory a developer edits by hand.
    let result = answer(&local_source(), "1.0.0", Some(&cached("v9.9.9")));
    assert_eq!(
        result,
        UpdateCheckResult::Unavailable {
            reason: REASON_LOCAL_SOURCE.to_string()
        }
    );
}

#[test]
fn a_release_source_with_nothing_cached_yet_is_no_answer_yet() {
    let result = answer(&release_source(), "1.0.0", None);
    assert_eq!(
        result,
        UpdateCheckResult::Unavailable {
            reason: REASON_NO_ANSWER_YET.to_string()
        }
    );
}

#[test]
fn a_cached_tag_that_does_not_parse_as_a_version_is_no_answer_yet() {
    let result = answer(&release_source(), "1.0.0", Some(&cached("not-a-version")));
    assert_eq!(
        result,
        UpdateCheckResult::Unavailable {
            reason: REASON_NO_ANSWER_YET.to_string()
        }
    );
}

#[test]
fn a_newer_cached_release_answers_ok_with_update_available() {
    let result = answer(&release_source(), "1.1.0", Some(&cached("v1.2.0")));
    assert_eq!(
        result,
        UpdateCheckResult::Ok {
            current: "1.1.0".to_string(),
            latest: "1.2.0".to_string(),
            update_available: true,
            release_url: "https://github.com/owner/repo/releases/tag/v1.2.0".to_string(),
            notes: "release notes".to_string(),
        }
    );
}

#[test]
fn an_equal_cached_release_answers_ok_with_no_update_available() {
    let result = answer(&release_source(), "1.2.0", Some(&cached("v1.2.0")));
    assert_eq!(
        result,
        UpdateCheckResult::Ok {
            current: "1.2.0".to_string(),
            latest: "1.2.0".to_string(),
            update_available: false,
            release_url: "https://github.com/owner/repo/releases/tag/v1.2.0".to_string(),
            notes: "release notes".to_string(),
        }
    );
}

#[test]
fn an_installed_version_newer_than_anything_published_is_ok_not_an_error() {
    // The boundary CONTRACT.md names explicitly: `update_available: false`,
    // not a refusal — a developer testing an unreleased version must not see
    // this route error out from under them.
    let result = answer(&release_source(), "2.0.0", Some(&cached("v1.2.0")));
    assert_eq!(
        result,
        UpdateCheckResult::Ok {
            current: "2.0.0".to_string(),
            latest: "1.2.0".to_string(),
            update_available: false,
            release_url: "https://github.com/owner/repo/releases/tag/v1.2.0".to_string(),
            notes: "release notes".to_string(),
        }
    );
}

#[test]
fn the_answer_is_the_shape_the_station_already_serves() {
    let json =
        serde_json::to_value(answer(&release_source(), "1.0.0", None)).expect("it serialises");

    assert_eq!(json["status"], "unavailable");
    assert_eq!(json["reason"], REASON_NO_ANSWER_YET);
    assert_eq!(
        json.as_object().expect("an object").len(),
        2,
        "no field beyond status and reason: {json}"
    );
}

#[test]
fn the_ok_shape_carries_exactly_the_five_fields() {
    let json = serde_json::to_value(answer(&release_source(), "1.1.0", Some(&cached("v1.2.0"))))
        .expect("it serialises");

    assert_eq!(json["status"], "ok");
    assert_eq!(
        json.as_object().expect("an object").len(),
        6,
        "status plus the five answer fields: {json}"
    );
}

#[test]
fn a_dev_context_is_always_unavailable_local_source() {
    // No registry entry exists for a `dev` session, so there is nothing to
    // look up — `Context::Dev` answers the same way a local archive does,
    // just without a `Source` to consult in the first place.
    let result = answer_now(&Context::Dev);
    assert_eq!(
        result,
        UpdateCheckResult::Unavailable {
            reason: REASON_LOCAL_SOURCE.to_string()
        }
    );
}

#[test]
fn an_installed_context_with_nothing_cached_is_no_answer_yet() {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    let context = Context::Installed {
        source: release_source(),
        app_version: "1.0.0".to_string(),
        cache_path: paths.update_cache_path(),
    };

    let result = answer_now(&context);
    assert_eq!(
        result,
        UpdateCheckResult::Unavailable {
            reason: REASON_NO_ANSWER_YET.to_string()
        }
    );
}

#[test]
fn an_installed_context_reads_the_cache_fresh_off_disk() {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    crate::update_cache::update(&paths, |cache| {
        cache.insert("owner/repo".to_string(), cached("v1.2.0"));
    })
    .expect("it writes");
    let context = Context::Installed {
        source: release_source(),
        app_version: "1.1.0".to_string(),
        cache_path: paths.update_cache_path(),
    };

    let result = answer_now(&context);
    assert_eq!(
        result,
        UpdateCheckResult::Ok {
            current: "1.1.0".to_string(),
            latest: "1.2.0".to_string(),
            update_available: true,
            release_url: "https://github.com/owner/repo/releases/tag/v1.2.0".to_string(),
            notes: "release notes".to_string(),
        }
    );
}

#[test]
fn local_archive_has_no_update_feed() {
    let mut source = local_source();
    source.kind = crate::registry::SourceKind::LocalArchive;
    let result = answer(&source, "1.0.0", Some(&cached("v9.9.9")));
    assert_eq!(
        result,
        UpdateCheckResult::Unavailable {
            reason: REASON_LOCAL_SOURCE.to_string()
        }
    );
}
