use super::render;
use crate::registry::{
    now_timestamp, Platform, ReferenceKind, Registry, RegistryEntry, Source, SourceKind, State,
};

fn entry(id: &str) -> RegistryEntry {
    RegistryEntry {
        id: id.to_string(),
        identifier: format!("dev.local.{id}"),
        source: Source {
            kind: SourceKind::LocalArchive,
            location: "/releases/demo-0.1.0.tar.gz".to_string(),
            reference: None,
            reference_kind: None,
            index: None,
        },
        app_version: "0.6.0".to_string(),
        source_revision: "sha256:1111".to_string(),
        app_port: None,
        platform: Platform {
            php_version: "8.5".to_string(),
            extensions_hash: "a1b2c3d4".repeat(8),
        },
        state: State::Ready,
        installed_at: now_timestamp(),
        updated_at: now_timestamp(),
        unknown: serde_json::Map::new(),
    }
}

fn registry(entries: impl IntoIterator<Item = RegistryEntry>) -> Registry {
    Registry {
        apps: entries.into_iter().collect(),
        ..Registry::default()
    }
}

#[test]
fn an_empty_registry_is_an_inventory_not_an_error() {
    // The state of every machine that has never installed anything, which is
    // the first one a new user's `list` meets.
    assert_eq!(render(&registry([])), "no apps installed\n");
}

#[test]
fn a_ready_app_shows_what_it_is_and_where_it_came_from() {
    let text = render(&registry([entry("tfsapp-test")]));
    let line = text.lines().nth(1).expect("one app line");

    for expected in [
        "tfsapp-test",
        "dev.local.tfsapp-test",
        "0.6.0",
        "ready",
        "8.5+a1b2c3d4",
        "/releases/demo-0.1.0.tar.gz",
    ] {
        assert!(
            line.contains(expected),
            "{expected:?} missing from {line:?}"
        );
    }
    // Nothing is off about it, so nothing is said about it.
    assert!(!line.contains('('), "no markers on a quiet app: {line:?}");
}

#[test]
fn columns_line_up_across_apps_of_different_name_lengths() {
    let text = render(&registry([entry("a"), entry("a-much-longer-id")]));
    let mut lines = text.lines();
    let header = lines
        .next()
        .and_then(|line| line.find("identifier"))
        .expect("a header");
    let columns: Vec<usize> = std::iter::once(header)
        .chain(lines.map(|line| line.find("dev.local.").expect("an identifier")))
        .collect();

    assert!(
        columns.windows(2).all(|pair| pair[0] == pair[1]),
        "the identifier column wanders:\n{text}"
    );
}

#[test]
fn a_release_source_shows_its_tag_and_nothing_off_about_it() {
    let mut entry = entry("demo");
    entry.source = Source {
        kind: SourceKind::Release,
        location: "owner/demo".to_string(),
        reference: Some("v1.4.0".to_string()),
        reference_kind: Some(ReferenceKind::Tag),
        index: Some("github".to_string()),
    };

    let text = render(&registry([entry]));

    assert!(text.contains("owner/demo@v1.4.0"), "{text}");
    assert!(!text.contains('('), "no markers on a quiet app: {text:?}");
}

#[test]
fn a_state_reads_the_way_the_registry_file_spells_it() {
    // One vocabulary for the file and the CLI: a user comparing the two must
    // not have to translate.
    let mut entry = entry("demo");
    entry.state = State::NeedsRevalidation;

    let text = render(&registry([entry]));

    assert!(text.contains("needs-revalidation"), "{text}");
}
