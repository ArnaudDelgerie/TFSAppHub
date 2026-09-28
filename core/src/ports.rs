use std::{fs, net::TcpListener, path::Path};

use serde::Deserialize;

/// The data dir's own persisted `data/config.json` (see CONTRACT.md §6):
/// records which app version last wrote the data dir, so a mismatched
/// binary can be caught before it touches data written by another version.
///
/// Ported from the station's `config.rs` along with its only reader below —
/// this is the *data* dir's config, not the project's `tfsapp.config.json`
/// manifest. It lands here rather than in a `config.rs` of its own because
/// `resolve_packaged_port` is so far its only consumer on this side; the
/// station's other readers (`lifecycle`, `run`, `portability`) arrive with
/// the plans that need them, and moving it then is that plan's call. The
/// file is §6, so it means the same thing on both hosts — the same
/// `identifier` resolves to the same data dir either way, which is exactly
/// what makes a packaged install openable from the hub.
#[derive(Deserialize, serde::Serialize)]
pub struct DataConfig {
    pub version: String,
    /// Per-installation escape hatch for a static `app_port` that's already
    /// taken on this machine (see CONTRACT.md §6). Only meaningful when
    /// `app_port` is set; ignored when it's `null`. Never auto-written —
    /// stays absent until a user adds it by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_override: Option<u16>,
    /// Per-installation revocation of a declared capability (decision 007's
    /// dated revision, plan 070): `"revoked": {"media": {"microphone": true}}`
    /// makes the hub deny every capture request from this installation and
    /// report `TFS_MEDIA_MICROPHONE=0` although the manifest declares the
    /// microphone. Hand-edited exactly like `port_override` — never
    /// auto-written, stays absent until a user adds it by hand.
    #[serde(default, skip_serializing_if = "Revoked::is_empty")]
    pub revoked: Revoked,
}

/// What one installation has revoked: nothing by default, so `{}`,
/// `{"media": {}}` and an absent key all mean "nothing revoked". Only the
/// microphone is read from it today (plan 070); the shape leaves room for
/// other capabilities without a shape change.
#[derive(Deserialize, serde::Serialize, Default, Debug, PartialEq, Eq)]
pub struct Revoked {
    #[serde(default, skip_serializing_if = "RevokedMedia::is_empty")]
    pub media: RevokedMedia,
}

impl Revoked {
    pub fn is_empty(&self) -> bool {
        self.media.is_empty()
    }
}

/// The `media` members one installation has revoked. `microphone: false` —
/// like every absent member — means "not revoked".
#[derive(Deserialize, serde::Serialize, Default, Debug, PartialEq, Eq)]
pub struct RevokedMedia {
    #[serde(default)]
    pub microphone: bool,
}

impl RevokedMedia {
    pub fn is_empty(&self) -> bool {
        !self.microphone
    }
}

/// Resolve the port packaged mode should actually bind: `port_override` from
/// `data/config.json` if present, else the baked `app_port`, else `None`
/// (dynamic). `port_override` is only consulted when `app_port` is static —
/// it has no effect otherwise, per CONTRACT.md §6.
pub fn resolve_packaged_port(
    app_port: Option<u16>,
    data_subdir: &Path,
) -> Result<Option<u16>, Box<dyn std::error::Error>> {
    let Some(app_port) = app_port else {
        return Ok(None);
    };
    let config_file = data_subdir.join("config.json");
    let port_override = fs::read_to_string(&config_file)
        .ok()
        .and_then(|contents| serde_json::from_str::<DataConfig>(&contents).ok())
        .and_then(|config| config.port_override);
    Ok(Some(port_override.unwrap_or(app_port)))
}

/// Pre-`Builder` packaged-mode port-conflict guard, sibling to
/// `detect_lifecycle_event` and run with the same GTK-init-ordering
/// constraint (see that function's doc and `main`'s comment). Plan 031:
/// binds the port `launch` already carries (resolved once by
/// `resolve_packaged_env`) instead of resolving it a second time — `app_port`
/// is passed separately only to decide the dynamic-port no-op, since the
/// resolved `port` itself doesn't record whether it came from a static bake
/// or `pick_free_local_port`. No-op in dynamic-port mode (`app_port`
/// absent). On conflict, returns an error for the caller to dialog and abort
/// the launch with — naming the conflicting port and `data/config.json`, so
/// the user can add/edit `port_override` there and relaunch.
///
/// Port note: the station passes the whole `&PackagedEnv` its
/// `resolve_packaged_env` built, and reads exactly two of its fields. That
/// struct is a station-shaped bundle (it carries a `SecretStore`, an update
/// cache and a `tauri`-resolved resource path) which will never exist below
/// this boundary, so the two fields are parameters here instead. Same
/// checks, same message, same failure — the caller assembles the arguments
/// rather than the struct.
pub fn check_packaged_port(
    app_port: Option<u16>,
    port: u16,
    data_subdir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if app_port.is_none() {
        return Ok(());
    }
    if let Err(error) = bind_check_port(port) {
        let config_file = data_subdir.join("config.json");
        let message = format!(
            "Static port {port} is already in use: {error}. Add or edit \
             \"port_override\" in {} to pick a different port, then relaunch.",
            config_file.display()
        );
        return Err(message.into());
    }
    Ok(())
}

pub fn pick_free_local_port() -> Result<u16, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Bind `127.0.0.1:<port>` and immediately drop the listener, to confirm a
/// static port is free before FrankenPHP is asked to use it. Shared by dev
/// mode (fail-fast) and packaged mode (blocking popup + `port_override`).
pub fn bind_check_port(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    TcpListener::bind(("127.0.0.1", port))
        .map(|_| ())
        .map_err(|error| format!("port {port} is already in use: {error}").into())
}

#[cfg(test)]
#[path = "ports_tests.rs"]
mod tests;
