//! Deep-research panel — integration invariant suite.
//!
//! Phase 1 / Task 1 seeds this file with a real assertion of the kill-switch
//! env parser. Task 7 extends it with the router-level invariant tests
//! (off-by-default golden, fail-closed budget, quorum, served==rows,
//! multi-provider) built over the crate's mock-provider harness.

use tt_core::panel_enabled_from_env;

/// The `TT_PANEL_ENABLED` kill-switch parser: truthy only for `"1"` or a
/// case-insensitive `"true"`; absent or anything else is off (the panel is
/// off-by-default). Saves and restores the prior env value so adding more
/// tests to this binary later does not leak global state.
#[test]
fn tt_panel_enabled_env_parsing() {
    let key = "TT_PANEL_ENABLED";
    let prior = std::env::var(key).ok();

    std::env::remove_var(key);
    assert!(!panel_enabled_from_env(), "absent => off by default");

    std::env::set_var(key, "1");
    assert!(panel_enabled_from_env(), "\"1\" => on");

    std::env::set_var(key, "TRUE");
    assert!(panel_enabled_from_env(), "case-insensitive true => on");

    std::env::set_var(key, "yes");
    assert!(!panel_enabled_from_env(), "non-truthy => off");

    match prior {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}
