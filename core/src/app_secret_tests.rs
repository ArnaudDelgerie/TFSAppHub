// The station's `secret_key_allowed` and `resolve_app_secret` tests are not
// here: both functions stay in `hub/` until plan 007 brings `secrets.rs`
// across (see this module's own doc comment). What survives below is the
// whole of what ported: the generator, and the resolution *order* itself.

use super::*;

// --- random_secret_hex -----------------------------------------------------

#[test]
fn random_secret_hex_length_and_charset() {
    let secret = random_secret_hex().unwrap();
    assert_eq!(secret.len(), 64);
    assert!(secret
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}

#[test]
fn random_secret_hex_is_not_constant() {
    assert_ne!(random_secret_hex().unwrap(), random_secret_hex().unwrap());
}

// --- app_secret_action -------------------------------------------------

#[test]
fn app_secret_action_existing_keyring_entry_wins_over_file() {
    assert_eq!(
        app_secret_action(Some("from-keyring"), Some("from-file")),
        AppSecretAction::UseKeyring("from-keyring".to_string())
    );
}

#[test]
fn app_secret_action_migrates_file_when_no_keyring_entry() {
    assert_eq!(
        app_secret_action(None, Some("from-file")),
        AppSecretAction::Migrate("from-file".to_string())
    );
}

#[test]
fn app_secret_action_trims_and_empty_filters_file_value() {
    assert_eq!(
        app_secret_action(None, Some("  from-file  \n")),
        AppSecretAction::Migrate("from-file".to_string())
    );
    assert_eq!(
        app_secret_action(None, Some("   \n")),
        AppSecretAction::Generate
    );
    assert_eq!(app_secret_action(None, Some("")), AppSecretAction::Generate);
}

#[test]
fn app_secret_action_generates_when_neither_exists() {
    assert_eq!(app_secret_action(None, None), AppSecretAction::Generate);
}

// --- load_or_create_app_secret ---------------------------------------------
//
// The station covers this function only through `resolve_app_secret`'s
// keyring-unavailable branch, whose test needs a `SecretStore` and therefore
// stays behind until 007. These two exercise it directly instead, so the one
// ported function that actually writes a secret to disk does not cross the
// boundary uncovered — same assertions as the branch they stand in for, plus
// the `0600` the doc comment promises.

#[test]
fn load_or_create_app_secret_reuses_an_existing_file_and_tightens_its_mode() {
    let data_subdir = tempfile::tempdir().unwrap();
    let secret_file = data_subdir.path().join("app.secret");
    fs::write(&secret_file, "already-on-disk").unwrap();
    fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o644)).unwrap();

    let secret = load_or_create_app_secret(data_subdir.path()).unwrap();

    // The existing value is neither regenerated nor rewritten...
    assert_eq!(secret, "already-on-disk");
    assert_eq!(fs::read_to_string(&secret_file).unwrap(), "already-on-disk");
    // ...but a laxer mode left by a pre-plan installation is tightened.
    let mode = fs::metadata(&secret_file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn load_or_create_app_secret_generates_a_0600_file_and_reuses_it_next_launch() {
    let data_subdir = tempfile::tempdir().unwrap();
    let secret_file = data_subdir.path().join("app.secret");

    let secret = load_or_create_app_secret(data_subdir.path()).unwrap();

    assert_eq!(secret.len(), 64);
    assert_eq!(fs::read_to_string(&secret_file).unwrap(), secret);
    let mode = fs::metadata(&secret_file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // Stable across a second launch against the same data dir — persisted,
    // not regenerated.
    assert_eq!(
        load_or_create_app_secret(data_subdir.path()).unwrap(),
        secret
    );
}
