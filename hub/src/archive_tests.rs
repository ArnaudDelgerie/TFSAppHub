use std::{fs, path::PathBuf};

use flate2::{write::GzEncoder, Compression};
use tar::{Builder, EntryType, Header};

use super::{extract, extract_prefix, ArchiveError};

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

/// `git archive` always prefixes its output with a PAX global extended
/// header (a `comment=<commit hash>` record) — this is what any app author
/// building the plan's release archive "by hand" actually gets, and what
/// this fixture reproduces.
fn append_pax_global_header(builder: &mut Builder<GzEncoder<fs::File>>) {
    let content = b"52 comment=0123456789abcdef0123456789abcdef01234567\n";
    let mut header = Header::new_ustar();
    header.set_entry_type(EntryType::XGlobalHeader);
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_path("pax_global_header").expect("a valid path");
    header.set_cksum();
    builder
        .append(&header, &content[..])
        .expect("append pax global header");
}

#[test]
fn a_git_archive_style_pax_global_header_is_ignored() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "from-git-archive.tar.gz", |builder| {
        append_pax_global_header(builder);
        append_dir(builder, "demo-1.0.0/");
        append_file(builder, "demo-1.0.0/composer.json", b"{}");
    });

    let destination = temp.path().join("out");
    let root = extract(&archive, &destination).expect("a clean extraction");

    assert_eq!(root, destination.join("demo-1.0.0"));
    assert_eq!(
        fs::read_to_string(root.join("composer.json")).expect("composer.json on disk"),
        "{}"
    );
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

// --- extract_prefix (plan 022, `import`'s own extractor) -------------------

#[test]
fn extract_prefix_writes_only_entries_under_the_prefix_stripped() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "export.tar.gz", |builder| {
        append_file(builder, "manifest.json", b"{}");
        append_file(builder, "data/app.db", b"sqlite");
        append_file(builder, "data/app.db-wal", b"wal");
    });

    let destination = temp.path().join("out");
    extract_prefix(&archive, "data", &destination).expect("a clean extraction");

    assert_eq!(
        fs::read_to_string(destination.join("app.db")).expect("app.db on disk"),
        "sqlite"
    );
    assert_eq!(
        fs::read_to_string(destination.join("app.db-wal")).expect("app.db-wal on disk"),
        "wal"
    );
    // `manifest.json` sits outside the prefix — never written at all, not
    // even under some other name.
    assert!(!destination.join("manifest.json").exists());
    assert!(!temp.path().join("out/manifest.json").exists());
}

#[test]
fn extract_prefix_skips_a_directory_entry_naming_the_prefix_itself() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "with-dir-entry.tar.gz", |builder| {
        append_dir(builder, "data/");
        append_file(builder, "data/app.db", b"sqlite");
    });

    let destination = temp.path().join("out");
    extract_prefix(&archive, "data", &destination).expect("the bare prefix entry is skipped");

    assert_eq!(
        fs::read_to_string(destination.join("app.db")).expect("app.db on disk"),
        "sqlite"
    );
}

#[test]
fn extract_prefix_refuses_a_parent_dir_entry_even_outside_the_prefix() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "traversal.tar.gz", |builder| {
        append_raw_path(builder, "../evil", b"pwned");
        append_file(builder, "data/app.db", b"sqlite");
    });

    let error = extract_prefix(&archive, "data", &temp.path().join("out"))
        .expect_err("a traversal entry, even one that would fall outside the prefix");
    assert!(matches!(error, ArchiveError::UnsafePath { .. }), "{error}");
}

#[test]
fn extract_prefix_refuses_a_symlink_under_the_prefix_that_escapes_the_destination() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "symlink.tar.gz", |builder| {
        append_symlink(builder, "data/evil", "/etc/passwd");
    });

    let error = extract_prefix(&archive, "data", &temp.path().join("out"))
        .expect_err("an escaping symlink");
    assert!(matches!(error, ArchiveError::UnsafeLink { .. }), "{error}");
}

#[test]
fn extract_prefix_accepts_a_symlink_under_the_prefix_that_stays_inside_it() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let archive = write_archive(temp.path(), "internal-symlink.tar.gz", |builder| {
        append_file(builder, "data/app.db", b"sqlite");
        append_symlink(builder, "data/alias.db", "app.db");
    });

    let destination = temp.path().join("out");
    extract_prefix(&archive, "data", &destination).expect("a benign internal symlink");

    let link = destination.join("alias.db");
    assert!(fs::symlink_metadata(&link)
        .expect("the symlink was written")
        .is_symlink());
}
