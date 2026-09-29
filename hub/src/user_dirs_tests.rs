use std::{ffi::OsString, path::PathBuf};

use super::resolve;
use crate::manifest::PathsActions;

#[test]
fn a_resolved_member_outside_utf8_is_injected_as_its_raw_bytes() {
    use std::os::unix::ffi::OsStringExt;

    let paths = PathsActions {
        downloads: true,
        ..Default::default()
    };
    let raw = OsString::from_vec(b"/home/fake/T\xc3\xa9l\xc3\xa9chargements/\xff".to_vec());

    let directory = PathBuf::from(raw.clone());
    let resolved = resolve(&paths, move |_| Some(directory.clone()));
    assert_eq!(
        resolved,
        vec![("TFS_USER_DOWNLOADS_DIR", raw)],
        "a non-UTF-8 user directory must reach the app as its own bytes, never \
         as a lossy look-alike path"
    );
}

#[test]
fn a_declared_and_resolved_member_reports_its_variable() {
    let paths = PathsActions {
        downloads: true,
        ..Default::default()
    };

    let resolved = resolve(&paths, |_| Some(PathBuf::from("/home/fake/Downloads")));

    assert_eq!(
        resolved,
        vec![("TFS_USER_DOWNLOADS_DIR", "/home/fake/Downloads".into())]
    );
}

#[test]
fn a_declared_and_unresolved_member_omits_its_variable() {
    let paths = PathsActions {
        downloads: true,
        ..Default::default()
    };

    let resolved = resolve(&paths, |_| None);

    assert!(resolved.is_empty(), "{resolved:?}");
}

#[test]
fn an_undeclared_member_omits_its_variable_even_when_resolvable() {
    let paths = PathsActions::default();

    let resolved = resolve(&paths, |_| Some(PathBuf::from("/home/fake/anything")));

    assert!(resolved.is_empty(), "{resolved:?}");
}

#[test]
fn several_declared_members_each_report_independently() {
    let paths = PathsActions {
        downloads: true,
        pictures: true,
        videos: true,
        ..Default::default()
    };

    let resolved = resolve(&paths, |directory| match directory {
        glib::UserDirectory::Downloads => Some(PathBuf::from("/home/fake/Downloads")),
        glib::UserDirectory::Pictures => None,
        glib::UserDirectory::Videos => Some(PathBuf::from("/home/fake/Videos")),
        _ => panic!("undeclared member {directory:?} must never be looked up"),
    });

    assert_eq!(
        resolved,
        vec![
            ("TFS_USER_DOWNLOADS_DIR", "/home/fake/Downloads".into()),
            ("TFS_USER_VIDEOS_DIR", "/home/fake/Videos".into()),
        ],
        "pictures is declared but unresolved, so it must be absent, not empty"
    );
}

type DeclareOneMember = fn(&mut PathsActions);

#[test]
fn every_member_resolves_to_its_own_variable_name() {
    let cases: [(DeclareOneMember, &str); 8] = [
        (|p| p.desktop = true, "TFS_USER_DESKTOP_DIR"),
        (|p| p.documents = true, "TFS_USER_DOCUMENTS_DIR"),
        (|p| p.downloads = true, "TFS_USER_DOWNLOADS_DIR"),
        (|p| p.music = true, "TFS_USER_MUSIC_DIR"),
        (|p| p.pictures = true, "TFS_USER_PICTURES_DIR"),
        (|p| p.public_share = true, "TFS_USER_PUBLIC_SHARE_DIR"),
        (|p| p.templates = true, "TFS_USER_TEMPLATES_DIR"),
        (|p| p.videos = true, "TFS_USER_VIDEOS_DIR"),
    ];

    for (declare, expected_var) in cases {
        let mut paths = PathsActions::default();
        declare(&mut paths);

        let resolved = resolve(&paths, |_| Some(PathBuf::from("/home/fake/dir")));

        assert_eq!(
            resolved,
            vec![(expected_var, "/home/fake/dir".into())],
            "declaring only this member must report exactly {expected_var}"
        );
    }
}
