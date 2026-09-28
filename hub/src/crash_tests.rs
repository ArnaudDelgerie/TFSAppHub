use super::{decide, render_crash_page, TerminationAction};
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

// --- render_crash_page (plan 058 step 2) ------------------------------------

#[test]
fn the_product_name_is_escaped() {
    let page = render_crash_page("<b>Evil & Co</b>", None, None, "https://app.local/");
    assert!(!page.contains("<b>Evil"));
    assert!(page.contains("&lt;b&gt;Evil &amp; Co&lt;/b&gt;"));
}

#[test]
fn the_reload_link_targets_the_dead_uri_and_escapes_it() {
    let page = render_crash_page("App", None, None, "https://app.local/a?x=1&y=2");
    assert!(page.contains("href=\"https://app.local/a?x=1&amp;y=2\""));
}

#[test]
fn declared_colours_become_custom_properties() {
    let page = render_crash_page(
        "App",
        Some("#112233"),
        Some("#eeeeee"),
        "https://app.local/",
    );
    assert!(page.contains("--splash-bg:#112233;"));
    assert!(page.contains("--splash-text:#eeeeee;"));
}

#[test]
fn a_colour_carrying_css_syntax_cannot_break_out_of_its_declaration() {
    let page = render_crash_page(
        "App",
        Some("red; } body { display:none"),
        None,
        "https://app.local/",
    );
    assert!(!page.contains("red; }"));
    assert!(!page.contains("display:none}"));
    assert_eq!(page.matches("red").count(), 1);
    assert!(page.contains("\\3b ")); // `;`
    assert!(page.contains("\\7b ")); // `{`
    assert!(page.contains("\\7d ")); // `}`
}

#[test]
fn undeclared_colours_leave_no_custom_property_and_keep_the_fallback() {
    let page = render_crash_page("App", None, None, "https://app.local/");
    assert!(page.contains(":root{}"));
    assert!(!page.contains("--splash-bg:"));
    assert!(!page.contains("--splash-text:"));
    assert!(page.contains("var(--splash-bg,#1e1e1e)"));
    assert!(page.contains("var(--splash-text,#e0e0e0)"));
}
