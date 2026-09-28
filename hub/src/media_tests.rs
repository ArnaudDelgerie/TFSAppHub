use std::sync::mpsc::sync_channel;
use std::time::Duration;

use super::{
    await_grant, decide, page_is_app_origin, user_media_denial_reason, MediaPermissionKind,
    MicrophoneAccess,
};
use tauri::Url;

/// Test-scale stand-in for `MICROPHONE_GRANT_DEADLINE`: long enough that a
/// report sent before the call is already waiting, short enough that a
/// silent-sender test still finishes in milliseconds.
const TEST_DEADLINE: Duration = Duration::from_millis(50);

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

// --- `MicrophoneAccess` (plan 070 step 1) -----------------------------------

#[test]
fn access_is_allowed_only_when_declared_and_not_revoked() {
    assert_eq!(MicrophoneAccess::of(true, false), MicrophoneAccess::Allowed);
}

#[test]
fn a_revoked_declaration_is_revoked() {
    assert_eq!(MicrophoneAccess::of(true, true), MicrophoneAccess::Revoked);
}

#[test]
fn undeclared_wins_over_revoked() {
    // An app that declared nothing has nothing to revoke: the two share a
    // platform state but not a cause, and the denial reason names the right
    // one.
    assert_eq!(
        MicrophoneAccess::of(false, true),
        MicrophoneAccess::Undeclared
    );
}

#[test]
fn undeclared_and_not_revoked_is_undeclared() {
    assert_eq!(
        MicrophoneAccess::of(false, false),
        MicrophoneAccess::Undeclared
    );
}

// --- `decide`: the full table -----------------------------------------------

#[test]
fn audio_only_is_allowed_only_when_allowed_and_on_the_app_origin() {
    assert!(decide(audio_only(), MicrophoneAccess::Allowed, true));
    assert!(!decide(audio_only(), MicrophoneAccess::Undeclared, true));
    assert!(!decide(audio_only(), MicrophoneAccess::Allowed, false));
    assert!(!decide(audio_only(), MicrophoneAccess::Undeclared, false));
}

#[test]
fn video_only_is_always_denied() {
    assert!(!decide(video_only(), MicrophoneAccess::Allowed, true));
    assert!(!decide(video_only(), MicrophoneAccess::Undeclared, true));
    assert!(!decide(video_only(), MicrophoneAccess::Allowed, false));
}

#[test]
fn audio_and_video_together_is_always_denied() {
    // WebKit offers no partial grant on a combined request — the whole
    // request is refused, not narrowed to its audio half.
    assert!(!decide(audio_and_video(), MicrophoneAccess::Allowed, true));
}

#[test]
fn device_info_is_allowed_under_the_same_two_conditions_as_audio() {
    assert!(decide(
        MediaPermissionKind::DeviceInfo,
        MicrophoneAccess::Allowed,
        true
    ));
    assert!(!decide(
        MediaPermissionKind::DeviceInfo,
        MicrophoneAccess::Undeclared,
        true
    ));
    assert!(!decide(
        MediaPermissionKind::DeviceInfo,
        MicrophoneAccess::Allowed,
        false
    ));
}

#[test]
fn every_other_kind_is_denied_even_when_allowed_and_on_origin() {
    assert!(!decide(
        MediaPermissionKind::Other,
        MicrophoneAccess::Allowed,
        true
    ));
}

#[test]
fn every_kind_is_denied_under_revoked() {
    // A revoked microphone follows §3's own rule — no `enable-media-stream`,
    // no grant — but through the deny path, so `hub.log` gets a reason.
    for kind in [
        audio_only(),
        video_only(),
        audio_and_video(),
        MediaPermissionKind::DeviceInfo,
        MediaPermissionKind::Other,
    ] {
        assert!(!decide(kind, MicrophoneAccess::Revoked, true), "{kind:?}");
    }
}

#[test]
fn every_kind_is_denied_under_undeclared() {
    for kind in [
        audio_only(),
        video_only(),
        audio_and_video(),
        MediaPermissionKind::DeviceInfo,
        MediaPermissionKind::Other,
    ] {
        assert!(
            !decide(kind, MicrophoneAccess::Undeclared, true),
            "{kind:?}"
        );
    }
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
        user_media_denial_reason(MicrophoneAccess::Undeclared, true, true, false),
        "this app does not declare actions.media.microphone"
    );
    assert_eq!(
        user_media_denial_reason(MicrophoneAccess::Allowed, false, true, false),
        "the request was not on the app's own origin"
    );
    assert_eq!(
        user_media_denial_reason(MicrophoneAccess::Allowed, true, true, true),
        "the request included video, and this group grants audio only"
    );
    assert_eq!(
        user_media_denial_reason(MicrophoneAccess::Allowed, true, false, false),
        "the request was not for an audio device"
    );
}

#[test]
fn a_revoked_microphone_has_its_own_denial_reason() {
    assert_eq!(
        user_media_denial_reason(MicrophoneAccess::Revoked, true, true, false),
        "the microphone is revoked in data/config.json"
    );
}

// --- `await_grant` (plan 069 step 2) ----------------------------------------
//
// The wait is the half `TFS_MEDIA_MICROPHONE` actually reports (decision 007
// §5): what the closure really did, or nothing within the deadline. Real
// channels throughout, so the table covers the closed channel and the
// window-closed races the same way the launch does.

#[test]
fn a_dispatch_that_was_never_scheduled_is_not_a_grant() {
    assert!(!await_grant(None, TEST_DEADLINE));
}

#[test]
fn a_true_report_received_in_time_is_a_grant() {
    let (sender, receiver) = sync_channel(1);
    sender
        .send(true)
        .expect("an empty channel accepts one report");
    drop(sender);

    assert!(await_grant(Some(receiver), TEST_DEADLINE));
}

#[test]
fn a_false_report_received_in_time_is_not_a_grant() {
    let (sender, receiver) = sync_channel(1);
    sender
        .send(false)
        .expect("an empty channel accepts one report");
    drop(sender);

    assert!(!await_grant(Some(receiver), TEST_DEADLINE));
}

#[test]
fn a_channel_closed_before_any_report_is_not_a_grant() {
    // The window closed before the closure ran, or the closure was never
    // scheduled and the receiver outlived the sender either way.
    let (sender, receiver) = sync_channel(1);
    drop(sender);

    assert!(!await_grant(Some(receiver), TEST_DEADLINE));
}

#[test]
fn a_sender_kept_alive_but_silent_is_not_a_grant_within_the_deadline() {
    let (sender, receiver) = sync_channel(1);
    let start = std::time::Instant::now();

    assert!(!await_grant(Some(receiver), TEST_DEADLINE));
    // The answer came from the deadline, not from a hang: it took at least
    // the deadline to arrive.
    assert!(start.elapsed() >= TEST_DEADLINE);
    drop(sender);
}
