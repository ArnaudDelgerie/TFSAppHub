use super::{decide, TerminationAction};
use webkit2gtk::WebProcessTerminationReason;

// `webkit2gtk::WebProcessTerminationReason` is a plain enum (no live WebKit
// process needed to construct it), unlike `webkit2gtk::PermissionRequest` in
// `media_tests.rs` — so unlike that module's classification table, this one
// is exercised directly rather than through a hand-rolled stand-in.

#[test]
fn a_crash_or_a_memory_limit_recovers() {
    assert_eq!(
        decide(WebProcessTerminationReason::Crashed),
        TerminationAction::Recover
    );
    assert_eq!(
        decide(WebProcessTerminationReason::ExceededMemoryLimit),
        TerminationAction::Recover
    );
}

#[test]
fn a_termination_the_hub_asked_for_is_not_a_crash() {
    assert_eq!(
        decide(WebProcessTerminationReason::TerminatedByApi),
        TerminationAction::Ignore
    );
}
