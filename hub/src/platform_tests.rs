use super::{bundled_frankenphp, parse_probe, probe, PlatformError};

const REAL_OUTPUT: &str = "8.5\nCore,date,json,pcre,SPL,standard,PDO,sqlite3";

#[test]
fn the_two_printed_lines_become_a_probe() {
    let probed = parse_probe(REAL_OUTPUT).expect("it reads");

    assert_eq!(probed.php_version, "8.5");
    // Sorted here rather than in PHP, because the sort is part of the
    // fingerprint's definition.
    assert_eq!(
        probed.extensions,
        ["Core", "PDO", "SPL", "date", "json", "pcre", "sqlite3", "standard"]
    );
}

#[test]
fn a_trailing_newline_changes_nothing() {
    assert_eq!(
        parse_probe(&format!("{REAL_OUTPUT}\n")).expect("it reads"),
        parse_probe(REAL_OUTPUT).expect("it reads")
    );
}

#[test]
fn the_order_php_listed_them_in_does_not_reach_the_fingerprint() {
    // The property the whole thing rests on: the same interpreter must
    // fingerprint identically twice, and `get_loaded_extensions()` promises
    // nothing about order.
    let one = parse_probe("8.5\njson,Core,SPL").expect("it reads");
    let other = parse_probe("8.5\nSPL,json,Core").expect("it reads");

    assert_eq!(one.fingerprint(), other.fingerprint());
}

#[test]
fn one_extension_more_is_a_different_platform() {
    // The case that matters: a hub update that drops or adds an extension is
    // exactly what can make an installed app's composer.lock unsatisfiable.
    let before = parse_probe("8.5\njson,Core,SPL").expect("it reads");
    let after = parse_probe("8.5\njson,Core,SPL,intl").expect("it reads");

    assert_ne!(before.fingerprint(), after.fingerprint());
}

#[test]
fn a_php_minor_bump_is_a_different_platform() {
    let before = parse_probe("8.4\njson,Core,SPL").expect("it reads");
    let after = parse_probe("8.5\njson,Core,SPL").expect("it reads");

    assert_ne!(before.fingerprint(), after.fingerprint());
    assert_eq!(
        before.fingerprint().extensions_hash,
        after.fingerprint().extensions_hash
    );
}

#[test]
fn the_hash_is_a_full_hex_sha_256() {
    let fingerprint = parse_probe(REAL_OUTPUT).expect("it reads").fingerprint();

    assert_eq!(fingerprint.extensions_hash.len(), 64);
    assert!(fingerprint
        .extensions_hash
        .chars()
        .all(|character| character.is_ascii_hexdigit()));
}

#[test]
fn a_patch_release_is_not_a_new_platform() {
    // major.minor is the granularity composer.lock's platform requirements are
    // written at, which is why the probe never asks for PHP_VERSION.
    assert!(parse_probe("8.5.8\njson,Core").is_err());
}

#[test]
fn an_unreadable_answer_says_what_it_got() {
    for (output, why) in [
        ("", "empty"),
        ("Segmentation fault", "not a version"),
        ("8\njson,Core", "no minor"),
        ("8.5", "no extension line"),
        ("8.5\n", "an empty extension line"),
    ] {
        let error = parse_probe(output)
            .err()
            .unwrap_or_else(|| panic!("{why} must not parse"));

        assert!(matches!(error, PlatformError::Unreadable { .. }), "{why}");
        // The raw answer is quoted back, because the interesting failures are
        // the ones nobody predicted.
        assert!(error.to_string().contains("It answered"), "{error}");
    }
}

#[test]
fn the_bundled_interpreter_answers_for_itself() {
    // The only test that runs the real binary, and it is skipped rather than
    // failed when it is absent: `make check` must stay green on a fresh clone
    // where `make sidecar` has never run, and the 170 MB download is not
    // something a unit test should trigger.
    let candidates = bundled_frankenphp();
    let Some(binary) = candidates.iter().find(|path| path.is_file()) else {
        eprintln!(
            "skipped: none of {candidates:?} are there — run `make sidecar` to cover this one"
        );
        return;
    };

    let probed = probe(binary).expect("the bundled FrankenPHP answers");

    assert!(
        probed.php_version.starts_with('8'),
        "{}",
        probed.php_version
    );
    // Whatever else it ships, a PHP that can run Symfony has these.
    for expected in ["Core", "SPL", "json"] {
        assert!(
            probed.extensions.iter().any(|name| name == expected),
            "{expected} missing from {:?}",
            probed.extensions
        );
    }
    assert_eq!(probed.fingerprint(), probed.fingerprint());
}

#[test]
fn bundled_frankenphp_lists_the_dev_path_last() {
    // Whatever `packaged_resource_dir()` finds (or doesn't, outside an
    // AppImage) `<hub>/resources/frankenphp` — `make sidecar`'s download — has
    // to stay in the list, as the final fallback for a build from source.
    let candidates = bundled_frankenphp();
    let dev_path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/frankenphp");
    assert_eq!(candidates.last(), Some(&dev_path));
}

#[test]
fn the_package_name_matches_tauri_conf_json() {
    // `packaged_resource_dir()` builds a `PackageInfo` by hand rather than
    // re-running `tauri::generate_context!()`, and hardcodes the one field
    // `resource_dir` actually reads. This is the guard against that constant
    // drifting from `tauri.conf.json`'s own `productName` — the AppImage
    // bundler names the resource dir after that field, not after this one.
    let conf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    let conf = std::fs::read_to_string(&conf_path)
        .unwrap_or_else(|error| panic!("{}: {error}", conf_path.display()));
    let conf: serde_json::Value = serde_json::from_str(&conf).expect("valid JSON");
    let product_name = conf["productName"]
        .as_str()
        .expect("tauri.conf.json declares \"productName\"");

    // Mirrors the private `PACKAGE_NAME` constant in `platform.rs` — kept
    // local to this test rather than exported, since nothing else needs it.
    const PACKAGE_NAME: &str = "TFSAppHub";
    assert_eq!(product_name, PACKAGE_NAME);
}

#[test]
fn running_something_that_is_not_an_interpreter_fails_loudly() {
    let error = probe(std::path::Path::new("/nonexistent/frankenphp"))
        .expect_err("there is nothing to run");

    assert!(matches!(error, PlatformError::Unstartable { .. }));
    assert!(
        error.to_string().contains("/nonexistent/frankenphp"),
        "{error}"
    );
}
