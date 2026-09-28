//! One shared way for tests to turn a project directory into a release
//! archive (plan 066): `release_of` walks the tree, reads its own manifest
//! for the name and version, and runs the same `publish::build_archive`
//! production runs, so an install or update test starts from the
//! checksummed `.tar.gz` plus `SHA256SUMS.txt` a real release carries.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// Serves tree blobs from memory, the way the in-process tests pin a tree
/// without a Git child.
pub(crate) struct LocalBlobs(pub(crate) HashMap<String, Vec<u8>>);

impl crate::publish::BlobSource for LocalBlobs {
    fn copy_blob(
        &mut self,
        object: &str,
        destination: &mut dyn Write,
    ) -> Result<u64, crate::git::GitError> {
        let bytes = &self.0[object];
        destination
            .write_all(bytes)
            .expect("an in-memory blob writes");
        Ok(bytes.len() as u64)
    }
}

/// Build the release archive of `project`'s current tree into `out`, and
/// return the `.tar.gz`'s path.
///
/// Every regular file under `project` becomes one tree entry, skipping
/// [`crate::source::EXCLUDED_FROM_HASH`] at the top level, exactly as
/// `publish` filters a tracked tree; a file with its owner-execute bit set
/// is `0o100755`, everything else `0o100644`. The archive's name and the
/// `SHA256SUMS.txt` beside it come from `publish::build_archive` itself.
pub(crate) fn release_of(project: &Path, out: &Path) -> PathBuf {
    let mut blobs = HashMap::new();
    let entries = walked_entries(project, Path::new(""), &mut blobs);
    let manifest = crate::manifest::load(project).expect("a readable tfsapp.config.json");
    crate::publish::build_archive(
        &entries,
        &mut LocalBlobs(blobs),
        &manifest.manifest.project_name,
        &manifest.manifest.app_version,
        out,
    )
    .expect("the release archive builds")
    .archive_path
}

fn walked_entries(
    directory: &Path,
    relative: &Path,
    blobs: &mut HashMap<String, Vec<u8>>,
) -> Vec<crate::git::TreeEntry> {
    let mut entries = Vec::new();
    let mut children: Vec<PathBuf> = fs::read_dir(directory)
        .expect("a readable directory")
        .map(|child| child.expect("a readable entry").path())
        .collect::<Vec<_>>();
    children.sort();

    for child in children {
        let Some(name) = child.file_name() else {
            continue;
        };
        if relative.as_os_str().is_empty()
            && crate::source::EXCLUDED_FROM_HASH
                .iter()
                .any(|excluded| name == *excluded)
        {
            continue;
        }
        let child_relative = relative.join(name);
        let metadata = fs::symlink_metadata(&child).expect("readable metadata");
        if metadata.is_dir() {
            entries.extend(walked_entries(&child, &child_relative, blobs));
        } else if metadata.is_file() {
            use std::os::unix::fs::PermissionsExt;

            let object = child_relative.display().to_string();
            blobs.insert(object.clone(), fs::read(&child).expect("a readable file"));
            entries.push(crate::git::TreeEntry {
                path: child_relative,
                mode: if metadata.permissions().mode() & 0o100 != 0 {
                    0o100755
                } else {
                    0o100644
                },
                object_id: object,
            });
        }
    }
    entries
}
