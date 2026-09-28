//! `tfsapp-hub list` — what is installed, and from where.
//!
//! The first command that reads the registry for real, and the one that has to
//! work on a machine where nothing is installed at all: that is the state every
//! new user's first `list` meets, and answering it with an error would be a bug
//! rather than a diagnostic.

use crate::{
    cli::{EXIT_FAILED, EXIT_OK},
    paths::Paths,
    registry::{self, Registry, Source},
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
            print!("{}", render(&registry));
            EXIT_OK
        }
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// The inventory as text.
pub fn render(registry: &Registry) -> String {
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
        "platform".to_string(),
        "source".to_string(),
    ]];

    for entry in &registry.apps {
        rows.push([
            entry.id.clone(),
            entry.identifier.clone(),
            entry.app_version.clone(),
            entry.state.to_string(),
            entry.platform.to_string(),
            describe_source(&entry.source),
        ]);
    }

    // The last column carries the markers and never needs padding, so only the
    // columns before it are measured.
    let widths: Vec<usize> = (0..5)
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
        text.push_str(&row[5]);
        text.push('\n');
    }
    text
}

/// The source column: where it came from, and at what ref.
fn describe_source(source: &Source) -> String {
    let mut described = source.location.clone();
    if let Some(reference) = &source.reference {
        described.push('@');
        described.push_str(reference);
    }
    described
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
