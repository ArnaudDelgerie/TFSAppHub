use std::{fs, path::PathBuf};

use flate2::{write::GzEncoder, Compression};
use tar::{Builder, EntryType, Header};

use super::{extract, ArchiveError};

fn write_archive(
    dir: &std::path::Path,
    name: &str,
    build: impl FnOnce(&mut Builder<GzEncoder<fs::File>>),
) -> PathBuf {
    let path = dir.join(name);
    let file = fs::File::create(&path).expect("create archive file");
    let mut builder = Builder::new(GzEncoder::new(file, Compression::fast()));
    build(&mut builder);
    builder
        .into_inner()
        .expect("finish tar layer")
        .finish()
        .expect("finish gzip layer");
    path
}

fn append_dir(builder: &mut Builder<GzEncoder<fs::File>>, path: &str) {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    header.set_path(path).expect("a valid directory path");
    header.set_cksum();
    builder
        .append(&header, std::io::empty())
        .expect("append dir");
}

fn append_file(builder: &mut Builder<GzEncoder<fs::File>>, path: &str, content: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_path(path).expect("a valid file path");
    header.set_cksum();
    builder.append(&header, content).expect("append file");
}

fn append_symlink(builder: &mut Builder<GzEncoder<fs::File>>, path: &str, target: &str) {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    header.set_path(path).expect("a valid symlink path");
    // Unlike a path, a link *target* is allowed to be absolute by this
    // crate's own builder — a real symlink can legitimately point anywhere,
    // and it is the extractor's job, not the archive format's, to decide
    // whether that is safe to honour. Exactly the case this module guards.
    header
        .set_link_name(target)
        .expect("a valid symlink target");
    header.set_cksum();
    builder
        .append(&header, std::io::empty())
        .expect("append symlink");
}

/// Append an entry whose path bypasses [`Header::set_path`]'s own
/// "relative, no `..`" validation — the only way to build a fixture archive
/// that this crate's *builder* would refuse to write honestly, needed to
/// prove this module's *extractor* refuses it too.
fn append_raw_path(builder: &mut Builder<GzEncoder<fs::File>>, raw_path: &str, content: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    {
        let name = &mut header.as_old_mut().name;
        for byte in name.iter_mut() {
            *byte = 0;
        }
        let bytes = raw_path.as_bytes();
        assert!(
            bytes.len() <= name.len(),
            "fixture path too long for a raw header"
        );
        name[..bytes.len()].copy_from_slice(bytes);
    }
    header.set_cksum();
    builder.append(&header, content).expect("append raw entry");
}

#[test]
fn a_clean_archive_round_trips_into_its_top_level_directory() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "clean.tar.gz", |builder| {
        append_dir(builder, "demo-1.0.0/");
        append_file(builder, "demo-1.0.0/composer.json", b"{}");
        append_dir(builder, "demo-1.0.0/src/");
        append_file(builder, "demo-1.0.0/src/main.php", b"<?php\n");
    });

    let destination = temp.path().join("out");
    let root = extract(&archive, &destination).expect("a clean extraction");

    assert_eq!(root, destination.join("demo-1.0.0"));
    assert_eq!(
        fs::read_to_string(root.join("composer.json")).expect("composer.json on disk"),
        "{}"
    );
    assert_eq!(
        fs::read_to_string(root.join("src/main.php")).expect("src/main.php on disk"),
        "<?php\n"
    );
}

#[test]
fn an_empty_archive_has_no_top_level_directory() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "empty.tar.gz", |_builder| {});

    let error = extract(&archive, &temp.path().join("out")).expect_err("nothing to extract");
    assert!(
        matches!(error, ArchiveError::NoTopLevelDirectory),
        "{error}"
    );
}

#[test]
fn a_tarbomb_with_no_shared_top_level_directory_is_refused() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "tarbomb.tar.gz", |builder| {
        append_file(builder, "a.txt", b"a");
        append_file(builder, "b.txt", b"b");
    });

    let error = extract(&archive, &temp.path().join("out")).expect_err("a tarbomb");
    assert!(
        matches!(error, ArchiveError::MultipleTopLevelDirectories { .. }),
        "{error}"
    );
}

#[test]
fn an_entry_with_a_parent_dir_component_is_refused() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "traversal.tar.gz", |builder| {
        append_dir(builder, "demo-1.0.0/");
        append_raw_path(builder, "demo-1.0.0/../../etc/evil", b"pwned");
    });

    let error = extract(&archive, &temp.path().join("out")).expect_err("a traversal entry");
    assert!(matches!(error, ArchiveError::UnsafePath { .. }), "{error}");
}

#[test]
fn an_absolute_entry_is_refused() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "absolute.tar.gz", |builder| {
        append_raw_path(builder, "/etc/evil", b"pwned");
    });

    let error = extract(&archive, &temp.path().join("out")).expect_err("an absolute entry");
    assert!(matches!(error, ArchiveError::UnsafePath { .. }), "{error}");
}

#[test]
fn a_symlink_escaping_the_extraction_root_is_refused() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "symlink.tar.gz", |builder| {
        append_dir(builder, "demo-1.0.0/");
        append_symlink(builder, "demo-1.0.0/evil", "/etc/passwd");
    });

    let error = extract(&archive, &temp.path().join("out")).expect_err("an escaping symlink");
    assert!(matches!(error, ArchiveError::UnsafeLink { .. }), "{error}");
}

#[test]
fn a_symlink_inside_the_tree_is_extracted() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "internal-symlink.tar.gz", |builder| {
        append_dir(builder, "demo-1.0.0/");
        append_file(builder, "demo-1.0.0/real.txt", b"hi");
        append_symlink(builder, "demo-1.0.0/alias.txt", "real.txt");
    });

    let destination = temp.path().join("out");
    let root = extract(&archive, &destination).expect("a benign internal symlink");

    let link = root.join("alias.txt");
    assert!(fs::symlink_metadata(&link)
        .expect("the symlink was written")
        .is_symlink());
    assert_eq!(
        fs::read_link(&link).expect("its target"),
        PathBuf::from("real.txt")
    );
}

#[test]
fn an_archive_with_two_top_level_directories_is_refused() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "two-dirs.tar.gz", |builder| {
        append_dir(builder, "demo-1.0.0/");
        append_file(builder, "demo-1.0.0/a.txt", b"a");
        append_dir(builder, "demo-2.0.0/");
        append_file(builder, "demo-2.0.0/b.txt", b"b");
    });

    let error = extract(&archive, &temp.path().join("out")).expect_err("two top-level directories");
    assert!(
        matches!(error, ArchiveError::MultipleTopLevelDirectories { .. }),
        "{error}"
    );
}
