//! Runtime identity: one binary, N applications.
//!
//! The station bakes `identifier` into `tauri.conf.json` at build time, one
//! app per binary. The hub cannot: it is a single binary serving N installed
//! apps, so everything that keys off the identifier — the GTK application id,
//! the D-Bus name, the single-instance key, `WM_CLASS`, and the WebKitGTK
//! website-data directory holding the app's cookies — has to follow a value
//! resolved at runtime instead.
//!
//! It does, and almost for free: Tauri reads the app id from the **runtime**
//! config when `Builder::run(context)` starts GTK, so mutating
//! `Context::config_mut().identifier` beforehand moves all of them at once.
//! Measured rather than assumed — the station's identity spike
//! (`../TFSAppWorkstation/.project/hub/001-identity-spike.md`, verdict GO,
//! 2026-08-07) launched two identities of one binary side by side on Wayland
//! and X11 and found two owned bus names, two window classes and two separate
//! `~/.local/share/<identifier>/` trees, with no cookie crossing in either
//! direction despite both windows sharing the `127.0.0.1` cookie origin.
//!
//! Everything in this module therefore has to happen **before**
//! `tauri::Builder::run`, and before anything else touches GTK. That is also
//! why the config mutation and `set_prgname` live in the same entry point:
//! they share one deadline, and splitting them is how one of them gets
//! forgotten.

/// The resolved identity of the app the hub is about to open.
///
/// Plan 004 builds this from the app's `tfsapp.config.json`; until then the
/// temporary `open --identity <id>` form in `main.rs` builds it from argv.
pub struct Identity {
    /// Reverse-DNS application identifier, e.g. `dev.tfsapp.test`. It is at
    /// once the GTK app id, the D-Bus name, the single-instance key, the
    /// `WM_CLASS`, and the name of the `~/.local/share/<identifier>/` tree
    /// the WebView keeps its cookies in — which is why the hub only has to
    /// carry this one value around.
    pub identifier: String,
    /// Human-readable name, shown in the window title. The generated
    /// `.desktop` entry's `Name=` must later come from this same field: the
    /// spike watched the two diverge (switcher entry and window title
    /// disagreeing), and one manifest field feeding both is the fix.
    pub product_name: String,
}

/// Apply `identity` to `context`, before `tauri::Builder::run(context)` and
/// before anything initialises GTK.
pub fn apply(identity: &Identity, context: &mut tauri::Context) {
    apply_to_config(identity, context.config_mut());
    set_prgname(&identity.identifier);
}

/// The config half: the three fields that turn the hub's baked identity into
/// this app's identity. Separated from [`apply`] only so it can be tested —
/// its sibling below needs a process, not a test.
fn apply_to_config(identity: &Identity, config: &mut tauri::utils::config::Config) {
    config.identifier = identity.identifier.clone();
    config.product_name = Some(identity.product_name.clone());
    // Set explicitly, never inherited. Tauri defaults it to `false`, and over
    // in the station it is switched on only inside `build-app.sh`'s merged
    // config — so a hub that assumed a sane default would get no GTK app id at
    // all, which costs both concurrent launching (one shared app id means
    // GApplication uniqueness refuses the second app) and window identity. The
    // spike had to set it for the same reason: without it, check 1 would have
    // passed for the wrong reason.
    config.app.enable_gtk_app_id = true;
}

/// The `WM_CLASS` half.
///
/// `enable_gtk_app_id` moves `_GTK_APPLICATION_ID` and the D-Bus name, but not
/// `WM_CLASS`: GDK derives that from `g_get_prgname()`, which defaults to the
/// executable name — so every app of one hub binary would report
/// `WM_CLASS="tfsapp-hub"`, and every per-app `.desktop` `StartupWMClass`
/// would match every app. GNOME does not need this (it matches
/// `_GTK_APPLICATION_ID` first, and gave the spike's probes distinct dock
/// icons with this line explicitly disabled), but other desktops and plain X11
/// window managers do.
///
/// `gdk::set_program_class` is not an alternative: the gtk-rs binding asserts
/// an initialised GTK main thread, which is exactly what does not exist this
/// early. `set_prgname` has no such requirement and must run before GTK init.
fn set_prgname(identifier: &str) {
    gtk::glib::set_prgname(Some(identifier));
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
