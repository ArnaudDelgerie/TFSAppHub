//! `tfsapp-hub list` — what is installed, from where, and whether it still
//! matches its source.
//!
//! The first command that reads the registry for real, and the one that has to
//! work on a machine where nothing is installed at all: that is the state every
//! new user's first `list` meets, and answering it with an error would be a bug
//! rather than a diagnostic.
//!
//! One marker earns its place on a line, answering a question `app_version`
//! alone cannot:
//!
//! - **`changed since install`** — the source tree no longer hashes to what was
//!   recorded when it was installed, which is the case of a developer who
//!   edited their project and did not bump `app_version`. Shown only when the
//!   source could actually be read; an unreachable source says nothing at all,
//!   because a false "changed" sends someone looking for an edit they never
//!   made.

use crate::{
    cli::{EXIT_FAILED, EXIT_OK},
    paths::Paths,
    registry::{self, Registry, Source},
    source::{self, Revision},
};

/// The whole command: resolve the paths, read the registry, print it.
pub fn run() -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match registry::load(&paths) {
        Ok(registry) => {
            print!("{}", render(&registry, source::current_revision));
            EXIT_OK
        }
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// The inventory as text.
///
/// `revision` is a parameter rather than a direct call so the whole rendering
/// — including the "changed since install" decision, which is the only part
/// with any judgement in it — is testable against fixture registries without a
/// source tree on disk.
pub fn render(registry: &Registry, revision: impl Fn(&Source) -> Revision) -> String {
    if registry.apps.is_empty() {
        // An empty inventory, not a failure. No hint pointing at `install`
        // either, while `install` is not implemented: telling someone what to
        // type next and then refusing it is worse than saying less.
        return "no apps installed\n".to_string();
    }

    let mut rows = vec![[
        "id".to_string(),
        "identifier".to_string(),
        "version".to_string(),
        "state".to_string(),
        "source".to_string(),
    ]];

    for entry in &registry.apps {
        rows.push([
            entry.id.clone(),
            entry.identifier.clone(),
            entry.app_version.clone(),
            entry.state.to_string(),
            describe_source(&entry.source, &entry.source_revision, &revision),
        ]);
    }

    // The last column carries the markers and never needs padding, so only the
    // four before it are measured.
    let widths: Vec<usize> = (0..4)
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut text = String::new();
    for row in &rows {
        for (column, width) in widths.iter().enumerate() {
            text.push_str(&format!("{:width$}  ", row[column], width = width));
        }
        text.push_str(&row[4]);
        text.push('\n');
    }
    text
}

/// The source column: where it came from, at what ref, and what is off about
/// it.
fn describe_source(
    source: &Source,
    installed_revision: &str,
    revision: impl Fn(&Source) -> Revision,
) -> String {
    let mut described = source.location.clone();
    if let Some(reference) = &source.reference {
        described.push('@');
        described.push_str(reference);
    }

    let mut markers = Vec::new();
    if let Revision::At(current) = revision(source) {
        if current != installed_revision {
            markers.push("changed since install");
        }
    }

    if !markers.is_empty() {
        described.push_str(&format!("  ({})", markers.join(", ")));
    }
    described
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
