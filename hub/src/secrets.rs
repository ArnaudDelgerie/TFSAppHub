//! Secret storage, and the one regression the hub could introduce here.
//!
//! A real OS keyring (Secret Service on Linux, through the `keyring` crate) when
//! one is actually reachable — probed once at startup, not assumed — with a
//! plaintext-file fallback for the real case where it is not: a headless Linux
//! box with no D-Bus Secret Service running. CONTRACT.md §6, ported from the
//! station.
//!
//! Failures are reported, never swallowed. Every store operation answers a
//! `Result`, and the transports turn an `Err` into `storage_failed` instead of
//! a plausible success — a delete that failed must not read back as "already
//! clean", and a corrupt fallback file must not be overwritten by the next
//! write. And every keyring call — the startup probe included — runs under one
//! deadline, `KEYRING_DEADLINE`, because the Secret Service is a separate
//! process reached over D-Bus and one that stops answering would otherwise
//! hold the launch or a request forever. `with_deadline` leaves the blocked
//! thread behind on purpose; its comment says why that is the right trade.
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
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

use keyring::Entry;
use tauri::Manager;

use tfsapp_core::app_secret::{app_secret_action, load_or_create_app_secret, AppSecretAction};

/// The deadline every production keyring call runs under — the startup probe
/// as a whole, and each store operation individually. Five seconds is long
/// for a healthy Secret Service (its answers are local D-Bus round trips,
/// milliseconds) and short for a launch the user is watching: a service this
/// slow is not one to hold the splash behind.
pub const KEYRING_DEADLINE: Duration = Duration::from_secs(5);

/// The shortened deadline test stores run under, so a stalled fake answers
/// `TimedOut` in ~100 ms instead of costing every test five seconds.
#[cfg(test)]
pub const TEST_KEYRING_DEADLINE: Duration = Duration::from_millis(100);

/// Run `f` on its own thread and wait no longer than `deadline` for it,
/// returning `None` on expiry.
///
/// **The thread is left behind on purpose.** A frozen Secret Service cannot be
/// cancelled — the blocked D-Bus call owns the thread until it answers — so
/// the only alternative to leaking it would be blocking the caller forever,
/// which is the launch-holding bug this exists to bound. One leaked thread
/// per wedged call is the accepted cost; a launch that falls back to the file
/// store does not call the keyring again.
pub(crate) fn with_deadline<T: Send + 'static>(
    deadline: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        // A failed send only means the caller already timed out and dropped
        // the receiver — never a second error to report.
        let _ = sender.send(f());
    });
    receiver.recv_timeout(deadline).ok()
}

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

/// Why a store operation failed. Its `Display` is what the transports' one
/// warning line prints, so it names a cause and never a key or a value.
///
/// `Keyring` never reaches the line verbatim: the keyring crate's own
/// `Display` prints the whole `Credential` on `Ambiguous`, which is more than
/// a log line should ever be trusted with — see the `Display` impl below.
#[derive(Debug)]
pub enum StorageError {
    Keyring(keyring::Error),
    Io(std::io::Error),
    /// A `secrets.json` that does not parse. The file is left untouched: a
    /// write over it would destroy every other secret it holds.
    Corrupt,
    /// The keyring did not answer within the store's deadline (plan 068).
    TimedOut,
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keyring(keyring::Error::Ambiguous(_)) => {
                write!(f, "the keyring matched more than one entry")
            }
            Self::Keyring(error) => write!(f, "keyring: {error}"),
            Self::Io(error) => write!(f, "io: {error}"),
            Self::Corrupt => write!(f, "the secrets file does not parse"),
            Self::TimedOut => write!(f, "the keyring did not answer within its deadline"),
        }
    }
}

impl std::error::Error for StorageError {}

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
        deadline: Duration,
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
    /// The mode covers the two failure shapes audit 024 cares about — every
    /// operation refused, and every operation wedged — and both run under
    /// the same deadline wrapper as the real backend, so a stalled fake
    /// answers `TimedOut` exactly the way a frozen Secret Service does.
    #[cfg(test)]
    FakeKeyring {
        service: String,
        entries: FakeKeyring,
        mode: FakeMode,
        deadline: Duration,
    },
}

/// What one fake keyring does to each operation. `Stalled` waits on a
/// condvar that is never notified: the operation's thread blocks for the rest
/// of the test, whatever the number of calls — which is the whole point, a
/// wedged call cannot be cancelled, only abandoned.
#[cfg(test)]
#[derive(Clone, Default)]
enum FakeMode {
    #[default]
    Working,
    Failing,
    Stalled(Arc<(Mutex<()>, std::sync::Condvar)>),
}

#[derive(Clone)]
pub struct SecretStore(Arc<Backend>);

/// A persistent, hub-owned keyring double for tests.  Callers can create
/// several stores from one instance to model the production service/account
/// namespace without changing the keyring crate's process-global builder.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct FakeKeyring(Arc<Mutex<HashMap<(String, String), String>>>);

#[cfg(test)]
impl FakeKeyring {
    pub fn store(&self, service: &str) -> SecretStore {
        self.store_in_mode(service, FakeMode::Working)
    }

    /// A store over the same entries that fails every operation, so tests can
    /// seed through [`Self::store`] and observe what a caller did or did not
    /// overwrite once the failing view refused everything.
    pub fn failing_store(&self, service: &str) -> SecretStore {
        self.store_in_mode(service, FakeMode::Failing)
    }

    /// A store over the same entries whose every operation blocks until the
    /// test ends — the frozen-Secret-Service shape, under the shortened test
    /// deadline.
    pub fn stalled_store(&self, service: &str) -> SecretStore {
        self.store_in_mode(
            service,
            FakeMode::Stalled(Arc::new((Mutex::new(()), std::sync::Condvar::new()))),
        )
    }

    fn store_in_mode(&self, service: &str, mode: FakeMode) -> SecretStore {
        SecretStore(Arc::new(Backend::FakeKeyring {
            service: service.to_string(),
            entries: self.clone(),
            mode,
            deadline: TEST_KEYRING_DEADLINE,
        }))
    }
}

#[cfg(test)]
pub(crate) fn fake_keyring_failure() -> StorageError {
    StorageError::Keyring(keyring::Error::NoStorageAccess(
        "the failing keyring refuses every operation".into(),
    ))
}

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
///
/// The probe — a full set/get/delete round trip — runs under
/// [`KEYRING_DEADLINE`] as a whole: a Secret Service that never answers costs
/// the launch one bounded wait and one warning line, not a splash held
/// forever. Its callers in `app_env.rs` are untouched by the injectable
/// variant below.
pub fn new_store(service: &str, data_subdir: &Path) -> SecretStore {
    new_store_with(service, data_subdir, KEYRING_DEADLINE, keyring_available)
}

/// The same store behind an injectable probe and deadline, so a test can
/// stall the probe or the service without D-Bus. The probe decides exactly
/// what `new_store`'s does: `true` is the keyring, `false` the file backend,
/// and a probe that outlives `deadline` the file backend plus the warning
/// line a frozen service costs.
pub fn new_store_with<P>(
    service: &str,
    data_subdir: &Path,
    deadline: Duration,
    probe: P,
) -> SecretStore
where
    P: Fn(&str) -> bool + Send + 'static,
{
    let path = data_subdir.join("secrets.json");
    let probe_service = service.to_string();
    let backend = match with_deadline(deadline, move || probe(&probe_service)) {
        Some(true) => Backend::Keyring {
            service: service.to_string(),
            deadline,
        },
        // A failed probe is the silent fallback it always was: a headless
        // Linux box is the normal case, not a warning.
        Some(false) => Backend::File {
            path,
            lock: Mutex::new(()),
        },
        None => {
            eprintln!(
                "tfsapp-hub: warning: the Secret Service did not answer within {}s; \
                 secrets fall back to {} for this launch",
                deadline.as_secs(),
                path.display()
            );
            Backend::File {
                path,
                lock: Mutex::new(()),
            }
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
        Backend::FakeKeyring { .. } => true,
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

/// A missing file is an empty map — the first launch's legitimate state.
/// Anything else the read can do wrong — permissions, I/O, a file that does
/// not parse — is an `Err`, so callers stop rather than write over it and
/// destroy every secret it holds.
fn read_file_map(path: &Path) -> Result<HashMap<String, String>, StorageError> {
    match fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(|_| StorageError::Corrupt),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(error) => Err(StorageError::Io(error)),
    }
}

/// Same-directory temp file plus `rename`, created `0600` directly rather than
/// written-then-chmodded — so the map is never briefly readable at the umask's
/// permissions, and a crash mid-write can only leave the old or the new contents
/// in full. Every step propagates: a failed write must be reported as one,
/// never answered as a success the next launch disproves.
fn write_file_map(path: &Path, map: &HashMap<String, String>) -> Result<(), StorageError> {
    // A map of `String`s cannot fail to serialise; `Corrupt` stands in for the
    // impossible case so the signature still says "never swallowed".
    let contents = serde_json::to_string(map).map_err(|_| StorageError::Corrupt)?;
    let mut name = path
        .file_name()
        .expect("the secret store path has a file name")
        .to_os_string();
    name.push(".tmp");
    let temporary = path.with_file_name(name);

    let mut file = create_0600(&temporary).map_err(StorageError::Io)?;
    file.write_all(contents.as_bytes())
        .map_err(StorageError::Io)?;
    file.sync_all().map_err(StorageError::Io)?;
    fs::rename(&temporary, path).map_err(StorageError::Io)?;
    Ok(())
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

/// One store operation under its deadline: an expiry is `TimedOut`, which
/// the transports answer like any other store failure — never a success and
/// never a silent absence.
fn with_store_deadline<T, F>(deadline: Duration, run: F) -> Result<T, StorageError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
{
    with_deadline(deadline, run).unwrap_or(Err(StorageError::TimedOut))
}

/// One fake operation under the store's deadline, in the fake's mode: refused,
/// wedged, or `working` run as the real backend would answer it.
///
/// `Stalled` waits on a condvar nobody notifies, in a loop so a spurious
/// wakeup cannot turn a wedged call into an answer. The thread is abandoned
/// by the deadline wrapper, exactly as a frozen Secret Service's would be.
#[cfg(test)]
fn fake_operation<T: Send + 'static>(
    mode: &FakeMode,
    deadline: Duration,
    working: impl FnOnce() -> T + Send + 'static,
) -> Result<T, StorageError> {
    let mode = mode.clone();
    with_store_deadline(deadline, move || match mode {
        FakeMode::Failing => Err(fake_keyring_failure()),
        FakeMode::Stalled(stall) => {
            let (lock, never_notified) = &*stall;
            let mut guard = lock.lock().expect("the secret store is not poisoned");
            loop {
                guard = never_notified
                    .wait(guard)
                    .expect("the secret store is not poisoned");
            }
        }
        FakeMode::Working => Ok(working()),
    })
}

/// `Ok(false)`: a keyring with no entry, or a file with none. `Err`: the
/// backend could not answer the question — never a silent `false`, which a
/// caller would read as "there is nothing there".
pub fn secrets_has(store: &SecretStore, account: &str) -> Result<bool, StorageError> {
    match store.0.as_ref() {
        Backend::Keyring { service, deadline } => {
            let service = service.clone();
            let account = account.to_string();
            with_store_deadline(*deadline, move || {
                let entry = Entry::new(&service, &account).map_err(StorageError::Keyring)?;
                match entry.get_password() {
                    Ok(_) => Ok(true),
                    Err(keyring::Error::NoEntry) => Ok(false),
                    Err(error) => Err(StorageError::Keyring(error)),
                }
            })
        }
        Backend::File { path, .. } => Ok(read_file_map(path)?.contains_key(account)),
        #[cfg(test)]
        Backend::FakeKeyring {
            service,
            entries,
            mode,
            deadline,
        } => {
            let key = (service.clone(), account.to_string());
            let entries = entries.clone();
            fake_operation(mode, *deadline, move || {
                entries
                    .0
                    .lock()
                    .expect("the secret store is not poisoned")
                    .contains_key(&key)
            })
        }
    }
}

/// `Ok(None)`: the keyring reports no entry, or the file does not hold the
/// key. `Err`: the read itself failed. A keyring `NoEntry` is a plain
/// absence, never an error — callers could not tell "never set" from
/// "cannot say" otherwise.
pub fn secrets_get(store: &SecretStore, account: &str) -> Result<Option<String>, StorageError> {
    match store.0.as_ref() {
        Backend::Keyring { service, deadline } => {
            let service = service.clone();
            let account = account.to_string();
            with_store_deadline(*deadline, move || {
                let entry = Entry::new(&service, &account).map_err(StorageError::Keyring)?;
                match entry.get_password() {
                    Ok(value) => Ok(Some(value)),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(error) => Err(StorageError::Keyring(error)),
                }
            })
        }
        Backend::File { path, .. } => Ok(read_file_map(path)?.get(account).cloned()),
        #[cfg(test)]
        Backend::FakeKeyring {
            service,
            entries,
            mode,
            deadline,
        } => {
            let key = (service.clone(), account.to_string());
            let entries = entries.clone();
            fake_operation(mode, *deadline, move || {
                entries
                    .0
                    .lock()
                    .expect("the secret store is not poisoned")
                    .get(&key)
                    .cloned()
            })
        }
    }
}

/// The write must be reported as it happened: a keyring that refuses, or a
/// file that cannot be written, is an `Err` — never a silent success the
/// next launch disproves. In the file backend, a read that fails aborts
/// before any write: overwriting a file the store could not read would
/// destroy every other secret it holds.
pub fn secrets_set(store: &SecretStore, account: &str, value: String) -> Result<(), StorageError> {
    match store.0.as_ref() {
        Backend::Keyring { service, deadline } => {
            let service = service.clone();
            let account = account.to_string();
            with_store_deadline(*deadline, move || {
                let entry = Entry::new(&service, &account).map_err(StorageError::Keyring)?;
                entry.set_password(&value).map_err(StorageError::Keyring)?;
                Ok(())
            })
        }
        Backend::File { path, lock } => {
            let _guard = lock.lock().expect("the secrets file lock is not poisoned");
            let mut map = read_file_map(path)?;
            map.insert(account.to_string(), value);
            write_file_map(path, &map)
        }
        #[cfg(test)]
        Backend::FakeKeyring {
            service,
            entries,
            mode,
            deadline,
        } => {
            let key = (service.clone(), account.to_string());
            let entries = entries.clone();
            fake_operation(mode, *deadline, move || {
                entries
                    .0
                    .lock()
                    .expect("the secret store is not poisoned")
                    .insert(key, value);
            })
        }
    }
}

/// `Ok(false)`: the key was never there — a plain absence, written nowhere.
/// `Err`: the backend failed, in which case the key must be assumed to still
/// be there; a token the app believes revoked can no longer survive a failed
/// delete as though it had been revoked.
pub fn secrets_delete(store: &SecretStore, account: &str) -> Result<bool, StorageError> {
    match store.0.as_ref() {
        Backend::Keyring { service, deadline } => {
            let service = service.clone();
            let account = account.to_string();
            with_store_deadline(*deadline, move || {
                let entry = Entry::new(&service, &account).map_err(StorageError::Keyring)?;
                match entry.delete_credential() {
                    Ok(()) => Ok(true),
                    Err(keyring::Error::NoEntry) => Ok(false),
                    Err(error) => Err(StorageError::Keyring(error)),
                }
            })
        }
        Backend::File { path, lock } => {
            let _guard = lock.lock().expect("the secrets file lock is not poisoned");
            let mut map = read_file_map(path)?;
            let existed = map.remove(account).is_some();
            if existed {
                write_file_map(path, &map)?;
            }
            Ok(existed)
        }
        #[cfg(test)]
        Backend::FakeKeyring {
            service,
            entries,
            mode,
            deadline,
        } => {
            let key = (service.clone(), account.to_string());
            let entries = entries.clone();
            fake_operation(mode, *deadline, move || {
                entries
                    .0
                    .lock()
                    .expect("the secret store is not poisoned")
                    .remove(&key)
                    .is_some()
            })
        }
    }
}

/// Resolve `APP_SECRET` (CONTRACT.md §6): an existing keyring entry always wins;
/// a pre-keyring `app.secret` file is migrated only in its absence; otherwise a
/// fresh secret is generated and stored.
///
/// The decision itself is `core`'s, with its own tests; this is the plumbing
/// around it. Every failure — no reachable keyring, a read that fails, a
/// write that will not verify — falls back to the plaintext file rather than
/// propagating: `APP_SECRET` resolution must never be the reason an app will
/// not open, because everything the app signs (CSRF tokens, remember-me
/// cookies) depends on getting *a* stable value.
///
/// A keyring read that fails short-circuits the decision entirely: the entry
/// may exist behind the failure, so [`AppSecretAction::Generate`] must not be
/// consulted and the keyring must not be written to. The existing file is the
/// only value this launch can trust.
pub fn resolve_app_secret(
    store: &SecretStore,
    data_subdir: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    if !is_keyring(store) {
        return load_or_create_app_secret(data_subdir);
    }

    let secret_file = data_subdir.join("app.secret");
    let file_value = fs::read_to_string(&secret_file).ok();
    let keyring_value = match secrets_get(store, APP_SECRET_ACCOUNT) {
        Ok(value) => value,
        Err(_) => return load_or_create_app_secret(data_subdir),
    };

    match app_secret_action(keyring_value.as_deref(), file_value.as_deref()) {
        AppSecretAction::UseKeyring(secret) => {
            // A leftover from an installation that migrated on an earlier
            // launch: safe to drop now the keyring is confirmed to hold it.
            let _ = fs::remove_file(&secret_file);
            Ok(secret)
        }
        AppSecretAction::Migrate(secret) => {
            // An `Err` at either the write or the verification falls back to
            // the file, exactly as a mismatch does: the file is only dropped
            // once the keyring is confirmed to hold the value.
            let written = secrets_set(store, APP_SECRET_ACCOUNT, secret.clone()).is_ok();
            let verified = written
                && secrets_get(store, APP_SECRET_ACCOUNT)
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some(secret.as_str());
            if verified {
                let _ = fs::remove_file(&secret_file);
                Ok(secret)
            } else {
                load_or_create_app_secret(data_subdir)
            }
        }
        AppSecretAction::Generate => {
            let secret = tfsapp_core::app_secret::random_secret_hex()?;
            let written = secrets_set(store, APP_SECRET_ACCOUNT, secret.clone()).is_ok();
            let verified = written
                && secrets_get(store, APP_SECRET_ACCOUNT)
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some(secret.as_str());
            if verified {
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
/// not appear here either. The first store error propagates: a settings screen
/// built on "nothing is set" behind a failing store is worse than no answer.
#[derive(serde::Serialize, Debug, Clone, PartialEq)]
pub struct SecretListEntry {
    key: String,
    set: bool,
}

pub fn secret_list_entries(
    store: &SecretStore,
    keys: &[String],
) -> Result<Vec<SecretListEntry>, StorageError> {
    let mut entries = Vec::new();
    for key in keys.iter().filter(|key| secret_key_allowed(keys, key)) {
        entries.push(SecretListEntry {
            key: key.clone(),
            set: secrets_has(store, key)?,
        });
    }
    Ok(entries)
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

/// The transports' one answer to a store failure: the operation did not
/// happen, which the app must not read as "the key is not there" or "the key
/// is gone". Checked last, after `KEY_NOT_DECLARED` and `value_too_large`,
/// which are answered without touching the store at all.
pub(crate) const STORAGE_FAILED: &str = "storage_failed";

/// Turn a store error into that answer, writing the one warning line a
/// failure costs: `tfsapp-hub: warning: secret store: <cause>`. It names the
/// cause and never the key or the value; a launch's stderr is `hub.log`
/// (`open::prepare_hub_log`). Shared by both transports, so one failure is
/// one line whatever the caller asked over.
pub(crate) fn storage_failed(error: &StorageError) -> &'static str {
    eprintln!("tfsapp-hub: warning: secret store: {error}");
    STORAGE_FAILED
}

// The five commands are thin wrappers over generic `*_for_window` helpers —
// the same shape `open_files.rs` uses — so a test can drive the whole path
// with a mock window: the window is still the whole address, resolved once.

pub(crate) fn secret_has_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
    key: &str,
) -> Result<bool, &'static str> {
    let store = store_for_window(window).ok_or(NO_STORE)?;
    if !secret_key_allowed(&keys_for_window(window), key) {
        return Err(KEY_NOT_DECLARED);
    }
    secrets_has(&store, key).map_err(|error| storage_failed(&error))
}

pub(crate) fn secret_get_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
    key: &str,
) -> Result<Option<String>, &'static str> {
    let store = store_for_window(window).ok_or(NO_STORE)?;
    if !secret_key_allowed(&keys_for_window(window), key) {
        return Err(KEY_NOT_DECLARED);
    }
    secrets_get(&store, key).map_err(|error| storage_failed(&error))
}

pub(crate) fn secret_set_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
    key: &str,
    value: &str,
) -> Result<(), &'static str> {
    let store = store_for_window(window).ok_or(NO_STORE)?;
    if !secret_key_allowed(&keys_for_window(window), key) {
        return Err(KEY_NOT_DECLARED);
    }
    if value.len() > MAX_SECRET_VALUE_BYTES {
        return Err("value_too_large");
    }
    secrets_set(&store, key, value.to_string()).map_err(|error| storage_failed(&error))
}

pub(crate) fn secret_delete_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
    key: &str,
) -> Result<bool, &'static str> {
    let store = store_for_window(window).ok_or(NO_STORE)?;
    if !secret_key_allowed(&keys_for_window(window), key) {
        return Err(KEY_NOT_DECLARED);
    }
    secrets_delete(&store, key).map_err(|error| storage_failed(&error))
}

pub(crate) fn secret_list_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
) -> Result<Vec<SecretListEntry>, &'static str> {
    let store = store_for_window(window).ok_or(NO_STORE)?;
    secret_list_entries(&store, &keys_for_window(window)).map_err(|error| storage_failed(&error))
}

#[tauri::command]
pub fn secret_has(window: tauri::Window, key: String) -> Result<bool, &'static str> {
    secret_has_for_window(&window, &key)
}

#[tauri::command]
pub fn secret_get(window: tauri::Window, key: String) -> Result<Option<String>, &'static str> {
    secret_get_for_window(&window, &key)
}

#[tauri::command]
pub fn secret_set(window: tauri::Window, key: String, value: String) -> Result<(), &'static str> {
    secret_set_for_window(&window, &key, &value)
}

#[tauri::command]
pub fn secret_delete(window: tauri::Window, key: String) -> Result<bool, &'static str> {
    secret_delete_for_window(&window, &key)
}

#[tauri::command]
pub fn secret_list(window: tauri::Window) -> Result<Vec<SecretListEntry>, &'static str> {
    secret_list_for_window(&window)
}

#[cfg(test)]
pub fn new_fake_keyring_store() -> SecretStore {
    new_fake_keyring().store("test.tfsapp-hub")
}

#[cfg(test)]
pub fn new_fake_keyring() -> FakeKeyring {
    FakeKeyring::default()
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
