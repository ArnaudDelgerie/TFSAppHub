//! `APP_SECRET` (CONTRACT.md §6), minus everything that needs a secret store.
//!
//! The station's `app_secret.rs` is two layers. The lower one — the three-way
//! decision, the plaintext-file fallback, the generator — is pure, and is what
//! this module is. The upper one is `resolve_app_secret`, which drives that
//! decision against a `secrets::SecretStore`, plus the `secret_key_allowed`
//! guard and the reserved-key list that the IPC commands and the HTTP bridge
//! enforce. All four read `crate::secrets`, so they arrive in `hub/` with
//! `secrets.rs` in plan 007, and come back down here with it whenever the
//! "move `secrets`/`bridge`/`worker` into `core/`" plan runs.
//!
//! That split is not a loss of fidelity: the resolution *order* the contract
//! promises — an existing keyring entry always wins, a pre-plan `app.secret`
//! file is migrated only in its absence, otherwise generate — lives entirely
//! in `app_secret_action` below, with its tests. `resolve_app_secret` is the
//! plumbing around it, and plumbing needs the pipe.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

/// The three-way decision `resolve_app_secret` makes, factored out as a pure
/// function of what's already there so it is unit-testable without a real
/// keyring (see `app_secret_tests.rs`): an existing keyring entry always wins; a
/// pre-plan `app.secret` file is migrated only in its absence; otherwise a
/// fresh secret is generated.
#[derive(Debug, PartialEq)]
pub enum AppSecretAction {
    UseKeyring(String),
    Migrate(String),
    Generate,
}

pub fn app_secret_action(keyring_value: Option<&str>, file_value: Option<&str>) -> AppSecretAction {
    if let Some(value) = keyring_value {
        AppSecretAction::UseKeyring(value.to_string())
    } else if let Some(value) = file_value.map(str::trim).filter(|value| !value.is_empty()) {
        AppSecretAction::Migrate(value.to_string())
    } else {
        AppSecretAction::Generate
    }
}

/// Fallback `APP_SECRET` path (CONTRACT.md §6): plaintext file, used when no
/// OS keyring is reachable. Read the persisted `APP_SECRET` from the data
/// dir, or generate and persist one on first run. Reused on every subsequent
/// launch.
///
/// `0600` is enforced idempotently: created with that mode directly (so
/// there is no window where the secret is briefly world/group-readable
/// under the umask), and tightened on every launch if an existing file was
/// left laxer (e.g. a pre-plan installation, or a manually-recreated file).
/// Directory `0700` on the data dir (`packaged_data_dir`) is the primary
/// barrier; this is defense in depth.
pub fn load_or_create_app_secret(data_subdir: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let secret_file = data_subdir.join("app.secret");

    if let Ok(existing) = fs::read_to_string(&secret_file) {
        let existing = existing.trim();
        if !existing.is_empty() {
            let metadata = fs::metadata(&secret_file)?;
            if metadata.permissions().mode() & 0o777 != 0o600 {
                fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o600))?;
            }
            return Ok(existing.to_string());
        }
    }

    let secret = random_secret_hex()?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&secret_file)?;
    file.write_all(secret.as_bytes())?;
    Ok(secret)
}

pub fn random_secret_hex() -> Result<String, Box<dyn std::error::Error>> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|error| format!("Cannot generate secret: {error}"))?;
    let mut secret = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        secret.push_str(&format!("{byte:02x}"));
    }
    Ok(secret)
}

#[cfg(test)]
#[path = "app_secret_tests.rs"]
mod tests;
