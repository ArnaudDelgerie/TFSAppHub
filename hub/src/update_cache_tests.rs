use super::{load, update, CachedRelease};
use crate::paths::Paths;

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

fn release(tag: &str) -> CachedRelease {
    CachedRelease {
        checked_at: "2026-08-11T00:00:00Z".to_string(),
        tag: tag.to_string(),
        release_url: format!("https://github.com/owner/repo/releases/tag/{tag}"),
        notes: "notes".to_string(),
        unknown: serde_json::Map::new(),
    }
}

#[test]
fn a_missing_file_is_an_empty_cache() {
    let (_base, paths) = temp_paths();

    assert!(load(&paths).is_empty());
}

#[test]
fn a_malformed_file_is_read_as_empty_rather_than_an_error() {
    let (_base, paths) = temp_paths();
    std::fs::create_dir_all(paths.hub_root()).expect("hub root");
    std::fs::write(paths.update_cache_path(), b"not json at all").expect("write garbage");

    assert!(load(&paths).is_empty());
}

#[test]
fn one_entry_survives_a_write_and_a_read() {
    let (_base, paths) = temp_paths();

    update(&paths, |cache| {
        cache.insert("owner/repo".to_string(), release("v1.2.0"));
    })
    .expect("it writes");

    let cache = load(&paths);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache["owner/repo"], release("v1.2.0"));
}

#[test]
fn two_repositories_sit_side_by_side() {
    let (_base, paths) = temp_paths();

    update(&paths, |cache| {
        cache.insert("owner/repo-a".to_string(), release("v1.0.0"));
    })
    .expect("first write");
    update(&paths, |cache| {
        cache.insert("owner/repo-b".to_string(), release("v2.0.0"));
    })
    .expect("second write");

    let cache = load(&paths);
    assert_eq!(cache.len(), 2);
    assert_eq!(cache["owner/repo-a"], release("v1.0.0"));
    assert_eq!(cache["owner/repo-b"], release("v2.0.0"));
}

#[test]
fn an_entry_s_unknown_fields_survive_a_read_modify_write() {
    let (_base, paths) = temp_paths();

    update(&paths, |cache| {
        let mut entry = release("v1.0.0");
        entry
            .unknown
            .insert("future_field".to_string(), serde_json::json!("kept"));
        cache.insert("owner/repo".to_string(), entry);
    })
    .expect("first write");

    // A second write that touches a different repo must not drop the first
    // entry's field a newer hub wrote and this one does not know.
    update(&paths, |cache| {
        cache.insert("owner/other".to_string(), release("v3.0.0"));
    })
    .expect("second write");

    let cache = load(&paths);
    assert_eq!(
        cache["owner/repo"].unknown.get("future_field"),
        Some(&serde_json::json!("kept"))
    );
}

#[test]
fn a_concurrent_write_never_loses_the_other_entry() {
    let (_base, paths) = temp_paths();
    let writers = 8;

    std::thread::scope(|scope| {
        for index in 0..writers {
            let paths = &paths;
            scope.spawn(move || {
                update(paths, |cache| {
                    cache.insert(format!("owner/repo-{index}"), release("v1.0.0"));
                })
                .expect("every writer succeeds");
            });
        }
    });

    let cache = load(&paths);
    let mut keys: Vec<&String> = cache.keys().collect();
    keys.sort();
    let expected: Vec<String> = (0..writers)
        .map(|index| format!("owner/repo-{index}"))
        .collect();
    assert_eq!(keys, expected.iter().collect::<Vec<_>>());
}
