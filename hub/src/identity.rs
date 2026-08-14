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
/// `manifest::Manifest::identity` builds it from the app's
/// `tfsapp.config.json`; both installed and live launches resolve that
/// manifest before opening their window.
#[derive(Debug)]
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
    /// PNG to use as the window icon, resolved inside the installed app's own
    /// directory. `None` when the app declares none — see [`load_icon`] for
    /// why that is not an error.
    pub icon_path: Option<std::path::PathBuf>,
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

/// Decode the app's window icon, for the window builder's `.icon()`.
///
/// This is the one identity-derived thing that is *not* free: the station
/// bakes `bundle.icon` at build time, and the hub has no per-app build step to
/// bake anything in, so the PNG is read from the installed app's directory at
/// launch. `tauri`'s `image-png` feature is what makes the decode available —
/// the station enables no image feature at all.
///
/// A missing or undecodable file warns and yields `None`: an app whose author
/// shipped a broken icon is still an app the user must be able to open, and
/// refusing to launch over a picture would be the wrong trade. It comes out as
/// a generic window icon, which is exactly what the user would have got had
/// the app declared no icon at all.
///
/// An icon larger than X11 will carry is downscaled rather than dropped, and
/// the fallback is announced — see [`fit_to_limit`].
pub fn load_icon(path: &std::path::Path) -> Option<tauri::image::Image<'static>> {
    let decoded = match tauri::image::Image::from_path(path) {
        Ok(image) => image,
        Err(error) => {
            eprintln!(
                "tfsapp-hub: opening without a window icon — cannot read {}: {error}",
                path.display()
            );
            return None;
        }
    };

    let (width, height) = (decoded.width(), decoded.height());
    // Copied out rather than borrowed: `Image::rgba(&'a self)` ties the borrow
    // to the image's own lifetime parameter, which cannot be `'static` for a
    // local. The copy is one icon, once, at launch.
    let rgba = decoded.rgba().to_vec();

    let (rgba, fitted_width, fitted_height) = fit_to_limit(rgba, width, height, MAX_ICON_WORDS);
    if (fitted_width, fitted_height) != (width, height) {
        eprintln!(
            "tfsapp-hub: window icon {} is {width}×{height}, larger than X11 will carry — \
             using it at {fitted_width}×{fitted_height}. The file itself is untouched.",
            path.display()
        );
    }

    Some(tauri::image::Image::new_owned(
        rgba,
        fitted_width,
        fitted_height,
    ))
}

/// The most an X11 window icon can be.
///
/// GDK writes `_NET_WM_ICON` as an array of `2 + width * height` words (the two
/// being the dimensions) and refuses to write it at all past
/// `GDK_SELECTION_MAX_SIZE`, which caps at 262144 words. It refuses in silence:
/// no warning, no error, the property is simply never set and the window comes
/// up with the window manager's generic icon. A 512×512 PNG — the size the
/// hub's own icon happens to be — needs 262146 words and misses by two.
///
/// Measured on this binary under `GDK_BACKEND=x11` (plan 003 step 4): at
/// 512×512 `xprop _NET_WM_ICON` reports `not found`, at 256×256 it reports
/// `256, 256, …`.
const MAX_ICON_WORDS: u64 = 262_144;

/// Whether `_NET_WM_ICON` can hold an icon of these dimensions.
fn fits(width: u32, height: u32, max_words: u64) -> bool {
    2 + u64::from(width) * u64::from(height) <= max_words
}

/// Downscale `rgba` until it fits `max_words`, returning it with its new
/// dimensions. Already-fitting icons and degenerate ones are returned as they
/// came.
///
/// Downscaling rather than dropping is the family rule for a value a host
/// cannot honour: fall back to the nearest one it can, and say so. Silence
/// would leave an app author believing they ship an icon they do not.
///
/// The resample is a box average over integer blocks — no dependency needed,
/// since `Image::from_path` has already handed us decoded RGBA. The block size
/// is the smallest that fits, so as much resolution as possible survives:
/// 512×512 halves to 256×256, 1024×1024 thirds to 342×342 rather than
/// quartering to 256×256. When the block divides the dimensions exactly every
/// output pixel is a whole block's average; where it does not, the last row and
/// column average the smaller block that is actually there. Alpha is
/// averaged alongside the colour channels rather than premultiplied, which can
/// darken a hard transparent edge slightly; for a window icon that is not worth
/// a dependency.
fn fit_to_limit(rgba: Vec<u8>, width: u32, height: u32, max_words: u64) -> (Vec<u8>, u32, u32) {
    if width == 0 || height == 0 || fits(width, height, max_words) {
        return (rgba, width, height);
    }

    // Terminates: at `factor == max(width, height)` the target is 1×1.
    let mut factor = 2;
    while !fits(width.div_ceil(factor), height.div_ceil(factor), max_words) {
        factor += 1;
    }

    let (target_width, target_height) = (width.div_ceil(factor), height.div_ceil(factor));
    let mut fitted = Vec::with_capacity((target_width as usize) * (target_height as usize) * 4);
    for target_y in 0..target_height {
        for target_x in 0..target_width {
            let (from_x, from_y) = (target_x * factor, target_y * factor);
            let (to_x, to_y) = ((from_x + factor).min(width), (from_y + factor).min(height));

            let mut channels = [0u64; 4];
            for y in from_y..to_y {
                for x in from_x..to_x {
                    let at = ((y as usize) * (width as usize) + (x as usize)) * 4;
                    for (channel, sum) in channels.iter_mut().enumerate() {
                        *sum += u64::from(rgba[at + channel]);
                    }
                }
            }

            let sampled = u64::from((to_x - from_x) * (to_y - from_y));
            for sum in channels {
                fitted.push((sum / sampled) as u8);
            }
        }
    }

    (fitted, target_width, target_height)
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
