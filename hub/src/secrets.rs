//! Secret storage, and the one regression the hub could introduce here.
//!
//! A real OS keyring (Secret Service on Linux, through the `keyring` crate) when
//! one is actually reachable — probed once at startup, not assumed — with a
//! plaintext-file fallback for the real case where it is not: a headless Linux
//! box with no D-Bus Secret Service running. CONTRACT.md §6, ported from the
//! station.
//!
//! **The store is resolved from the calling window, never from an argument.**
//! This is the hard rule of plan 007 and the one genuinely new risk the hub
//! creates. The station gets its `identifier` from being a one-app process and
//! could not get it wrong if it tried; the hub is one binary serving N apps, so
//! there is a version of this module — `secret_get(app_id: String, key: String)`
//! — that would look perfectly reasonable and would let any app's webview read
//! any other app's secrets by passing a string. It does not exist and must never
//! be written. Every command below takes `window: tauri::Window` and reaches the
//! store through it, so the answer is decided by which window asked, which the
//! webview does not get to choose.
//!
//! The process model already makes that resolution trivial — one process, one
//! app, one managed store — and that is the point: the safe shape is also the
//! simple one, and it stays safe if the process model ever changes.
//!
//! # The Secret Service is a namespace, not a security boundary
//!
//! Worth stating plainly, because the `service` string looks like isolation and
//! is not. Every entry below is stored under the app's `identifier` as the
//! service name, which keeps two apps from *colliding*. It does not keep them
//! from *reading each other*: the Secret Service authorises per login session,
//! so any process running as this user can ask for any service's entries. The
//! spike measured a cross-app read succeeding, and the same read succeeds today
//! between two installed station AppImages — this is not something the hub
//! introduces or could remove.
//!
//! Key prefixing would not fix it and is not attempted: the reader *lists*
//! entries, it does not guess their names. The only real fix is sandboxing the
//! processes, which is a change of distribution format and out of scope for the
//! hub. Said here and in the README's trust section, so nobody reads
//! per-`identifier` storage as per-app secrecy.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use keyring::Entry;
use tauri::Manager;

use tfsapp_core::app_secret::{app_secret_action, load_or_create_app_secret, AppSecretAction};

/// The keyring account `APP_SECRET` lives under, paired with the app's
/// `identifier` as the service — which is what makes a hubbed install of an app
/// find the very secret its packaged AppImage stored (CONTRACT.md §6).
pub const APP_SECRET_ACCOUNT: &str = "app-secret";

/// The account the startup probe round-trips against to find out whether a
/// keyring answers at all.
pub const PROBE_ACCOUNT: &str = "__tfsapp_availability_probe__";

/// Keys an `actions.secrets` consumer may never resolve, even when the app lists
/// them in its own `keys` by mistake: the app's own signing key, and the probe.
/// Enforced at the transport boundary — both transports — never inside the store
/// itself, which has no business knowing who is asking.
pub const RESERVED_SECRET_KEYS: &[&str] = &[APP_SECRET_ACCOUNT, PROBE_ACCOUNT];

/// The most a stored value may be, shared by both transports so nothing can
/// land in either backend beyond it. An API key is ~100 bytes and a PEM private
/// key ~3 KB, so this leaves real headroom without turning the keyring into a
/// data store.
pub const MAX_SECRET_VALUE_BYTES: usize = 8192;

/// The guard behind every IPC command and every bridge route: a reserved key is
/// always refused, and anything else must be explicitly declared. `keys` is a
/// manifest — typo-catching, enumerability, a cap on how many things an app can
/// ask for — and never what actually keeps apps or users apart.
pub fn secret_key_allowed(keys: &[String], key: &str) -> bool {
    if RESERVED_SECRET_KEYS.contains(&key) {
        return false;
    }
    keys.iter().any(|declared| declared == key)
}

enum Backend {
    Keyring {
        service: String,
    },
    /// Deliberately **not** encrypted. With no reachable keyring there is no
    /// secure place left to keep an encryption key, so encrypting the file would
    /// only be theatre. A degraded but functional mode, chosen over silently
    /// losing every secret on restart, and announced to the app through
    /// `TFS_KEYRING_AVAILABLE` (CONTRACT.md §3/§6) so it can tell its user.
    File {
        path: PathBuf,
        /// Serialises the whole read-modify-write. The bridge is
        /// thread-per-request, so two concurrent writers racing an unlocked
        /// cycle would silently drop one. One process owns one store, so
        /// intra-process locking is enough — no cross-process flock.
        lock: Mutex<()>,
    },
    /// Test-only stand-in for `Keyring` that never touches D-Bus, so the
    /// `APP_SECRET` resolution path can be exercised against a working
    /// "keyring" without one — and without ever writing to a developer's real
    /// login keyring, which is what calling `new_store` in a test would do.
    #[cfg(test)]
    FakeKeyring(Mutex<HashMap<String, String>>),
}

#[derive(Clone)]
pub struct SecretStore(Arc<Backend>);

/// One set/get/delete round trip against the real backend, run once at store
/// creation rather than lazily on first use, so every request afterwards
/// already knows which backend it is talking to.
fn keyring_available(service: &str) -> bool {
    let Ok(entry) = Entry::new(service, PROBE_ACCOUNT) else {
        return false;
    };
    if entry.set_password("probe").is_err() {
        return false;
    }
    let round_tripped = entry.get_password().ok().as_deref() == Some("probe");
    let _ = entry.delete_credential();
    round_tripped
}

/// Open the app's store. `service` is the app's `identifier` and `data_subdir`
/// the same `<data dir>/data` everything else writes into.
pub fn new_store(service: &str, data_subdir: &Path) -> SecretStore {
    let backend = if keyring_available(service) {
        Backend::Keyring {
            service: service.to_string(),
        }
    } else {
        Backend::File {
            path: data_subdir.join("secrets.json"),
            lock: Mutex::new(()),
        }
    };
    SecretStore(Arc::new(backend))
}

/// Which backend the probe picked. `APP_SECRET` resolution branches on it.
pub fn is_keyring(store: &SecretStore) -> bool {
    match store.0.as_ref() {
        Backend::Keyring { .. } => true,
        Backend::File { .. } => false,
        #[cfg(test)]
        Backend::FakeKeyring(_) => true,
    }
}

/// `TFS_KEYRING_AVAILABLE`'s value (CONTRACT.md §3/§6).
///
/// It reflects the backend actually in use, not whether a keyring is installed:
/// one that is present but locked or unreachable already fell back to the file
/// in [`new_store`], and reads as `"0"` here too. An app is entitled to warn its
/// user on that basis, which it could not do if this answered the question
/// "is a keyring installed".
pub fn keyring_env_value(store: &SecretStore) -> &'static str {
    if is_keyring(store) {
        "1"
    } else {
        "0"
    }
}

fn read_file_map(path: &Path) -> HashMap<String, String> {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

/// Same-directory temp file plus `rename`, created `0600` directly rather than
/// written-then-chmodded — so the map is never briefly readable at the umask's
/// permissions, and a crash mid-write can only leave the old or the new contents
/// in full.
fn write_file_map(path: &Path, map: &HashMap<String, String>) {
    let Ok(contents) = serde_json::to_string(map) else {
        return;
    };
    let mut name = path
        .file_name()
        .expect("the secret store path has a file name")
        .to_os_string();
    name.push(".tmp");
    let temporary = path.with_file_name(name);

    let written = create_0600(&temporary).and_then(|mut file| file.write_all(contents.as_bytes()));
    if written.is_ok() {
        let _ = fs::rename(&temporary, path);
    }
}

fn create_0600(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

pub fn secrets_has(store: &SecretStore, account: &str) -> bool {
    match store.0.as_ref() {
        Backend::Keyring { service } => Entry::new(service, account)
            .and_then(|entry| entry.get_password())
            .is_ok(),
        Backend::File { path, .. } => read_file_map(path).contains_key(account),
        #[cfg(test)]
        Backend::FakeKeyring(map) => map
            .lock()
            .expect("the secret store is not poisoned")
            .contains_key(account),
    }
}

pub fn secrets_get(store: &SecretStore, account: &str) -> Option<String> {
    match store.0.as_ref() {
        Backend::Keyring { service } => Entry::new(service, account).ok()?.get_password().ok(),
        Backend::File { path, .. } => read_file_map(path).get(account).cloned(),
        #[cfg(test)]
        Backend::FakeKeyring(map) => map
            .lock()
            .expect("the secret store is not poisoned")
            .get(account)
            .cloned(),
    }
}

pub fn secrets_set(store: &SecretStore, account: &str, value: String) {
    match store.0.as_ref() {
        Backend::Keyring { service } => {
            if let Ok(entry) = Entry::new(service, account) {
                let _ = entry.set_password(&value);
            }
        }
        Backend::File { path, lock } => {
            let _guard = lock.lock().expect("the secrets file lock is not poisoned");
            let mut map = read_file_map(path);
            map.insert(account.to_string(), value);
            write_file_map(path, &map);
        }
        #[cfg(test)]
        Backend::FakeKeyring(map) => {
            map.lock()
                .expect("the secret store is not poisoned")
                .insert(account.to_string(), value);
        }
    }
}

pub fn secrets_delete(store: &SecretStore, account: &str) -> bool {
    match store.0.as_ref() {
        Backend::Keyring { service } => Entry::new(service, account)
            .map(|entry| entry.delete_credential().is_ok())
            .unwrap_or(false),
        Backend::File { path, lock } => {
            let _guard = lock.lock().expect("the secrets file lock is not poisoned");
            let mut map = read_file_map(path);
            let existed = map.remove(account).is_some();
            if existed {
                write_file_map(path, &map);
            }
            existed
        }
        #[cfg(test)]
        Backend::FakeKeyring(map) => map
            .lock()
            .expect("the secret store is not poisoned")
            .remove(account)
            .is_some(),
    }
}

/// Resolve `APP_SECRET` (CONTRACT.md §6): an existing keyring entry always wins;
/// a pre-keyring `app.secret` file is migrated only in its absence; otherwise a
/// fresh secret is generated and stored.
///
/// The decision itself is `core`'s, with its own tests; this is the plumbing
/// around it. Every failure — no reachable keyring, a write that will not verify
/// — falls back to the plaintext file rather than propagating: `APP_SECRET`
/// resolution must never be the reason an app will not open, because everything
/// the app signs (CSRF tokens, remember-me cookies) depends on getting *a*
/// stable value.
pub fn resolve_app_secret(
    store: &SecretStore,
    data_subdir: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    if !is_keyring(store) {
        return load_or_create_app_secret(data_subdir);
    }

    let secret_file = data_subdir.join("app.secret");
    let file_value = fs::read_to_string(&secret_file).ok();
    let keyring_value = secrets_get(store, APP_SECRET_ACCOUNT);

    match app_secret_action(keyring_value.as_deref(), file_value.as_deref()) {
        AppSecretAction::UseKeyring(secret) => {
            // A leftover from an installation that migrated on an earlier
            // launch: safe to drop now the keyring is confirmed to hold it.
            let _ = fs::remove_file(&secret_file);
            Ok(secret)
        }
        AppSecretAction::Migrate(secret) => {
            secrets_set(store, APP_SECRET_ACCOUNT, secret.clone());
            if secrets_get(store, APP_SECRET_ACCOUNT).as_deref() == Some(secret.as_str()) {
                // Security over continuity: the plaintext file goes only once
                // the keyring write is confirmed, never before.
                let _ = fs::remove_file(&secret_file);
                Ok(secret)
            } else {
                load_or_create_app_secret(data_subdir)
            }
        }
        AppSecretAction::Generate => {
            let secret = tfsapp_core::app_secret::random_secret_hex()?;
            secrets_set(store, APP_SECRET_ACCOUNT, secret.clone());
            if secrets_get(store, APP_SECRET_ACCOUNT).as_deref() == Some(secret.as_str()) {
                Ok(secret)
            } else {
                load_or_create_app_secret(data_subdir)
            }
        }
    }
}

/// One entry per declared key, in declaration order, with whether it holds a
/// value — the enumeration CONTRACT.md §2 promises, and the only reliable way an
/// app can populate a settings screen. Filtered through [`secret_key_allowed`]
/// like every other entry point, so a reserved key an app listed by mistake does
/// not appear here either.
#[derive(serde::Serialize, Debug, Clone, PartialEq)]
pub struct SecretListEntry {
    key: String,
    set: bool,
}

pub fn secret_list_entries(store: &SecretStore, keys: &[String]) -> Vec<SecretListEntry> {
    keys.iter()
        .filter(|key| secret_key_allowed(keys, key))
        .map(|key| SecretListEntry {
            key: key.clone(),
            set: secrets_has(store, key),
        })
        .collect()
}

/// The store the window that asked owns.
///
/// **The whole point of this function is its argument.** It takes a window and
/// nothing else identifying — no app id, no identifier, no service name — so a
/// webview cannot name a store it does not own. The five commands below all go
/// through it, and none of them takes a parameter that could name another app.
///
/// `None` only before the launch has managed the store, which is a window that
/// cannot exist yet: the store is managed before the app window is navigated to
/// the backend.
pub fn store_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
) -> Option<tauri::State<'_, SecretStore>> {
    window.app_handle().try_state::<SecretStore>()
}

/// `actions.secrets`' declared keys, likewise resolved from the calling window.
pub fn keys_for_window<R: tauri::Runtime>(window: &tauri::Window<R>) -> Vec<String> {
    window
        .app_handle()
        .try_state::<crate::manifest::SecretsActions>()
        .map(|actions| actions.keys.clone())
        .unwrap_or_default()
}

/// A single stable error covering both an undeclared key and a reserved one —
/// deliberately indistinguishable from the caller's side, exactly as the store
/// is untouched in either case.
const KEY_NOT_DECLARED: &str = "key_not_declared";
const NO_STORE: &str = "unavailable";

#[tauri::command]
pub fn secret_has(window: tauri::Window, key: String) -> Result<bool, &'static str> {
    let store = store_for_window(&window).ok_or(NO_STORE)?;
    if !secret_key_allowed(&keys_for_window(&window), &key) {
        return Err(KEY_NOT_DECLARED);
    }
    Ok(secrets_has(&store, &key))
}

#[tauri::command]
pub fn secret_get(window: tauri::Window, key: String) -> Result<Option<String>, &'static str> {
    let store = store_for_window(&window).ok_or(NO_STORE)?;
    if !secret_key_allowed(&keys_for_window(&window), &key) {
        return Err(KEY_NOT_DECLARED);
    }
    Ok(secrets_get(&store, &key))
}

#[tauri::command]
pub fn secret_set(window: tauri::Window, key: String, value: String) -> Result<(), &'static str> {
    let store = store_for_window(&window).ok_or(NO_STORE)?;
    if !secret_key_allowed(&keys_for_window(&window), &key) {
        return Err(KEY_NOT_DECLARED);
    }
    if value.len() > MAX_SECRET_VALUE_BYTES {
        return Err("value_too_large");
    }
    secrets_set(&store, &key, value);
    Ok(())
}

#[tauri::command]
pub fn secret_delete(window: tauri::Window, key: String) -> Result<bool, &'static str> {
    let store = store_for_window(&window).ok_or(NO_STORE)?;
    if !secret_key_allowed(&keys_for_window(&window), &key) {
        return Err(KEY_NOT_DECLARED);
    }
    Ok(secrets_delete(&store, &key))
}

#[tauri::command]
pub fn secret_list(window: tauri::Window) -> Result<Vec<SecretListEntry>, &'static str> {
    let store = store_for_window(&window).ok_or(NO_STORE)?;
    Ok(secret_list_entries(&store, &keys_for_window(&window)))
}

#[cfg(test)]
pub fn new_fake_keyring_store() -> SecretStore {
    SecretStore(Arc::new(Backend::FakeKeyring(Mutex::new(HashMap::new()))))
}

/// A real `File`-backend store, bypassing [`new_store`]'s D-Bus probe, so a test
/// can exercise the fallback path whatever the machine running it has.
#[cfg(test)]
pub fn new_file_store_for_test(path: PathBuf) -> SecretStore {
    SecretStore(Arc::new(Backend::File {
        path,
        lock: Mutex::new(()),
    }))
}

#[cfg(test)]
#[path = "secrets_tests.rs"]
mod tests;
