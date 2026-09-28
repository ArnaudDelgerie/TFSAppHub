use super::{decide, page_is_app_origin, user_media_denial_reason, MediaPermissionKind};
use tauri::Url;

fn app_origin() -> Url {
    Url::parse("http://127.0.0.1:4321").expect("a valid origin")
}

fn audio_only() -> MediaPermissionKind {
    MediaPermissionKind::UserMedia {
        audio: true,
        video: false,
    }
}

fn video_only() -> MediaPermissionKind {
    MediaPermissionKind::UserMedia {
        audio: false,
        video: true,
    }
}

fn audio_and_video() -> MediaPermissionKind {
    MediaPermissionKind::UserMedia {
        audio: true,
        video: true,
    }
}

// --- `decide`: the full table -----------------------------------------------

#[test]
fn audio_only_is_allowed_only_when_declared_and_on_the_app_origin() {
    assert!(decide(audio_only(), true, true));
    assert!(!decide(audio_only(), false, true));
    assert!(!decide(audio_only(), true, false));
    assert!(!decide(audio_only(), false, false));
}

#[test]
fn video_only_is_always_denied() {
    assert!(!decide(video_only(), true, true));
    assert!(!decide(video_only(), false, true));
    assert!(!decide(video_only(), true, false));
}

#[test]
fn audio_and_video_together_is_always_denied() {
    // WebKit offers no partial grant on a combined request — the whole
    // request is refused, not narrowed to its audio half.
    assert!(!decide(audio_and_video(), true, true));
}

#[test]
fn device_info_is_allowed_under_the_same_two_conditions_as_audio() {
    assert!(decide(MediaPermissionKind::DeviceInfo, true, true));
    assert!(!decide(MediaPermissionKind::DeviceInfo, false, true));
    assert!(!decide(MediaPermissionKind::DeviceInfo, true, false));
}

#[test]
fn every_other_kind_is_denied_even_when_declared_and_on_origin() {
    assert!(!decide(MediaPermissionKind::Other, true, true));
}

// --- `page_is_app_origin` ----------------------------------------------------

#[test]
fn no_origin_published_yet_is_never_the_app_origin() {
    let page = app_origin();
    assert!(!page_is_app_origin(None, Some(&page)));
}

#[test]
fn the_same_origin_matches() {
    let origin = app_origin();
    let page = app_origin();
    assert!(page_is_app_origin(Some(&origin), Some(&page)));
}

#[test]
fn a_different_origin_does_not_match() {
    let origin = app_origin();
    let elsewhere = Url::parse("http://127.0.0.1:9999").expect("a valid origin");
    assert!(!page_is_app_origin(Some(&origin), Some(&elsewhere)));
}

#[test]
fn no_current_page_is_never_the_app_origin() {
    let origin = app_origin();
    assert!(!page_is_app_origin(Some(&origin), None));
}

// --- `user_media_denial_reason` ---------------------------------------------

#[test]
fn the_denial_reason_names_the_first_thing_that_failed() {
    assert_eq!(
        user_media_denial_reason(false, true, true, false),
        "this app does not declare actions.media.microphone"
    );
    assert_eq!(
        user_media_denial_reason(true, false, true, false),
        "the request was not on the app's own origin"
    );
    assert_eq!(
        user_media_denial_reason(true, true, true, true),
        "the request included video, and this group grants audio only"
    );
    assert_eq!(
        user_media_denial_reason(true, true, false, false),
        "the request was not for an audio device"
    );
}
