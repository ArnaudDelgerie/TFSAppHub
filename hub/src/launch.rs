//! `LaunchSpec`: what a window needs to open, independent of where it came from.
//!
//! `open <id>` and `dev <path>` (plan 009) are two ways to arrive at the same
//! launch — a sidecar started, guards run, a window shown — differing only in
//! *where the source is* and *where its state lives*. Before this module the
//! station's shape would have repeated here: two assembly functions, one per
//! mode, that never had to agree because they were compiled into different
//! AppImages. The hub is one binary, so that split would drift on the first
//! change either mode needs.
//!
//! `LaunchSpec` is the seam instead: one constructor per [`Source`] —
//! `open::resolve` builds the `Installed` one, `dev::resolve` (plan 009 step 2)
//! builds the `Live` one — and everything downstream (`main::prepare`,
//! `main::open_window`, `main::serve`, `app_env::resolve`, `sidecar::start`)
//! reads a `&LaunchSpec` and does not care which constructor built it.

// `Source::Live`, `state_root` and `label` have no reader yet: `open::resolve`
// is still this module's only constructor, and `main.rs` still re-derives its
// own data dir and still prints the raw `id` it was called with rather than a
// spec's `label`. Plan 009 steps 2–4 are their real consumers — the `dev`
// constructor, and the guards that read a spec instead of an installed `id`.
// Remove the allow as each lands, rather than letting it linger.
#![allow(dead_code)]

use std::path::PathBuf;

use crate::{identity::Identity, manifest::Manifest, update_check};

/// What varies between an installed snapshot and a live project: whether a
/// hub-local handle exists to re-execute against and to name in a message.
///
/// A `dev` session has no handle — it is not in the registry (see plan 009's
/// Overview, "Out of scope") — so [`Source::Live`] carries none. Everything
/// that *is* the same either way — the app directory, the identity, the state
/// root, the manifest — lives on [`LaunchSpec`] itself rather than being
/// duplicated per variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An installed snapshot, addressed by its hub-local handle.
    Installed { id: String },
    /// A live project, served in place.
    Live,
}

/// Everything a launch needs, resolved once and read by every step from
/// `main::prepare` to `sidecar::start`.
#[derive(Debug)]
pub struct LaunchSpec {
    pub source: Source,
    /// Where the app's own files are: the installed snapshot under the hub's
    /// root, or the live project's own directory.
    pub app_dir: PathBuf,
    pub identity: Identity,
    pub manifest: Manifest,
    /// Where this launch's state lives: the app's OS data dir for an
    /// installed snapshot, the live project's own `var/` for a dev session
    /// (plan 009 step 3). Named, not created — creating it is a later step's
    /// job, once the launch is actually going to run the app.
    pub state_root: PathBuf,
    /// What a message about this launch calls it: the hub-local handle for
    /// an installed app, the project path for a live source.
    pub label: String,
    /// What the manifest parse wanted to say. Carried out rather than printed
    /// where it is produced — see `open::Resolved`'s former doc, which said
    /// the same thing before this module existed.
    pub warnings: Vec<String>,
    /// What `actions.update`'s answer needs about this launch — see
    /// [`update_check::Context`]. `open::resolve` builds `Installed` from the
    /// registry entry it already read; `dev::resolve` builds `Dev`, since a
    /// live project has no entry to build one from.
    pub update: update_check::Context,
}

impl LaunchSpec {
    /// The hub-local handle, for the one caller that still needs it as a bare
    /// string — `open::child_args`, building the parent's re-exec argv.
    /// `None` for a live source, which has none to give.
    pub fn installed_id(&self) -> Option<&str> {
        match &self.source {
            Source::Installed { id } => Some(id),
            Source::Live => None,
        }
    }
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
