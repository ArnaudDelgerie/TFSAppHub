use super::*;

fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn sanitize_browser_env_is_a_no_op_outside_an_appimage() {
    // Dev launches never set any of these — the helper must not invent
    // removals or rewrites out of nothing.
    let diff = sanitize_browser_env(&env(&[("HOME", "/home/dev"), ("PATH", "/usr/bin")]));
    assert!(diff.remove.is_empty());
    assert!(diff.set.is_empty());
}

#[test]
fn sanitize_browser_env_removes_every_appdir_injected_variable_present() {
    let diff = sanitize_browser_env(&env(&[
        ("GTK_PATH", "x"),
        ("GTK_DATA_PREFIX", "x"),
        ("GTK_EXE_PREFIX", "x"),
        ("GTK_THEME", "x"),
        ("GTK_IM_MODULE_FILE", "x"),
        ("GDK_PIXBUF_MODULE_FILE", "x"),
        ("GIO_EXTRA_MODULES", "x"),
        ("GSETTINGS_SCHEMA_DIR", "x"),
        ("GDK_BACKEND", "x"),
        ("LD_LIBRARY_PATH", "x"),
        ("LD_PRELOAD", "x"),
    ]));
    for name in APPIMAGE_ENV_VARS_TO_REMOVE {
        assert!(diff.remove.contains(name), "expected {name} to be removed");
    }
    assert!(diff.set.is_empty());
}

#[test]
fn sanitize_browser_env_only_removes_variables_actually_present() {
    let diff = sanitize_browser_env(&env(&[("GTK_THEME", "Adwaita")]));
    assert_eq!(diff.remove, vec!["GTK_THEME"]);
}

#[test]
fn sanitize_browser_env_strips_only_appdir_rooted_xdg_data_dirs_entries() {
    let diff = sanitize_browser_env(&env(&[
        ("APPDIR", "/tmp/.mount_tfsapp"),
        (
            "XDG_DATA_DIRS",
            "/tmp/.mount_tfsapp/usr/share:/usr/share:/usr/local/share",
        ),
    ]));
    assert_eq!(
        diff.set,
        vec![("XDG_DATA_DIRS", "/usr/share:/usr/local/share".to_string())]
    );
    assert!(!diff.remove.contains(&"XDG_DATA_DIRS"));
}

#[test]
fn sanitize_browser_env_drops_xdg_data_dirs_entirely_when_every_entry_is_appdir_rooted() {
    let diff = sanitize_browser_env(&env(&[
        ("APPDIR", "/tmp/.mount_tfsapp"),
        ("XDG_DATA_DIRS", "/tmp/.mount_tfsapp/usr/share"),
    ]));
    assert!(diff.remove.contains(&"XDG_DATA_DIRS"));
    assert!(diff.set.is_empty());
}

#[test]
fn sanitize_browser_env_leaves_xdg_data_dirs_untouched_without_an_appdir() {
    // Dev-safe: APPDIR is never set outside an AppImage, so there is
    // nothing to strip — the value must pass through unchanged.
    let diff = sanitize_browser_env(&env(&[("XDG_DATA_DIRS", "/usr/share:/usr/local/share")]));
    assert!(diff.set.is_empty());
    assert!(!diff.remove.contains(&"XDG_DATA_DIRS"));
}

#[test]
fn sanitize_browser_env_matches_a_synthetic_appimage_environment() {
    // The exact shape `apprun-hooks/linuxdeploy-plugin-gtk.sh` produces
    // (read back from a built tfsapp-demo_0.2.0_amd64.AppImage, plan 054),
    // plus LD_LIBRARY_PATH the way AppRun.wrapped sets it.
    let appdir = "/tmp/.mount_tfsappVWXYZ";
    let diff = sanitize_browser_env(&env(&[
        ("APPDIR", appdir),
        ("GTK_DATA_PREFIX", appdir),
        ("GTK_THEME", "Adwaita:dark"),
        ("GDK_BACKEND", "wayland,x11"),
        (
            "XDG_DATA_DIRS",
            &format!("{appdir}/usr/share:/usr/share:/usr/share/ubuntu"),
        ),
        (
            "GSETTINGS_SCHEMA_DIR",
            &format!("{appdir}/usr/share/glib-2.0/schemas"),
        ),
        ("GTK_EXE_PREFIX", &format!("{appdir}/usr")),
        (
            "GTK_PATH",
            &format!("{appdir}/usr/lib/x86_64-linux-gnu/gtk-3.0:/usr/lib64/gtk-3.0"),
        ),
        (
            "GTK_IM_MODULE_FILE",
            &format!("{appdir}/usr/lib/x86_64-linux-gnu/gtk-3.0/3.0.0/immodules.cache"),
        ),
        (
            "GDK_PIXBUF_MODULE_FILE",
            &format!("{appdir}/usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders.cache"),
        ),
        (
            "GIO_EXTRA_MODULES",
            &format!("{appdir}/usr/lib/x86_64-linux-gnu/gio/modules"),
        ),
        (
            "LD_LIBRARY_PATH",
            &format!("{appdir}/usr/lib/:{appdir}/usr/lib/x86_64-linux-gnu/"),
        ),
    ]));

    // LD_PRELOAD is never actually set by either the hook or AppRun.wrapped
    // — only LD_LIBRARY_PATH is — so this only checks the variables this
    // synthetic environment actually declares.
    for name in APPIMAGE_ENV_VARS_TO_REMOVE
        .iter()
        .filter(|name| **name != "LD_PRELOAD")
    {
        assert!(diff.remove.contains(name), "expected {name} to be removed");
    }
    assert!(!diff.remove.contains(&"LD_PRELOAD"));
    assert_eq!(
        diff.set,
        vec![("XDG_DATA_DIRS", "/usr/share:/usr/share/ubuntu".to_string())]
    );
}
