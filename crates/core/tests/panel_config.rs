//! Unit-level tests for `routes::panel` — config types, header parsing, and
//! `tt_extras.panel` resolution. Run with:
//!   cargo test -p tt-core --test panel_config

use axum::http::HeaderMap;
use tt_core::routes::panel::{panel_from_header, ArbiterStrategyKind};

// ---------------------------------------------------------------------------
// Header-parse tests (Step 1 / Step 2)
// ---------------------------------------------------------------------------

#[test]
fn header_absent_is_none() {
    assert!(panel_from_header(&HeaderMap::new()).is_none());
}

#[test]
fn header_synthesize_parses() {
    let mut h = HeaderMap::new();
    h.insert("x-tokentrimmer-panel", "synthesize".parse().unwrap());
    assert!(matches!(
        panel_from_header(&h),
        Some(ArbiterStrategyKind::Synthesize)
    ));
}

#[test]
fn header_unknown_strategy_is_none() {
    let mut h = HeaderMap::new();
    h.insert("x-tokentrimmer-panel", "bogus".parse().unwrap());
    assert!(panel_from_header(&h).is_none());
}

#[test]
fn header_best_of_n_parses() {
    let mut h = HeaderMap::new();
    h.insert("x-tokentrimmer-panel", "best-of-n".parse().unwrap());
    assert!(matches!(
        panel_from_header(&h),
        Some(ArbiterStrategyKind::BestOfN)
    ));
}

#[test]
fn header_majority_parses() {
    let mut h = HeaderMap::new();
    h.insert("x-tokentrimmer-panel", "majority".parse().unwrap());
    assert!(matches!(
        panel_from_header(&h),
        Some(ArbiterStrategyKind::Majority)
    ));
}

// ---------------------------------------------------------------------------
// PanelConfig::resolve tests (Step 6)
// ---------------------------------------------------------------------------

use tt_core::routes::panel::{ModelRef, PanelConfig, PanelDefaults};
use tt_core::ApiError;
use tt_shared::messages::PanelExtras;

fn default_with_members() -> PanelDefaults {
    PanelDefaults {
        members: vec![
            ModelRef {
                model: "gpt-4o".to_string(),
                provider: None,
            },
            ModelRef {
                model: "claude-3-5-sonnet".to_string(),
                provider: None,
            },
        ],
        arbiter_model: ModelRef {
            model: "gpt-4o".to_string(),
            provider: None,
        },
    }
}

fn empty_defaults() -> PanelDefaults {
    PanelDefaults {
        members: vec![],
        arbiter_model: ModelRef {
            model: "gpt-4o".to_string(),
            provider: None,
        },
    }
}

#[test]
fn resolve_header_only_uses_defaults() {
    let cfg = PanelConfig::resolve(
        ArbiterStrategyKind::Synthesize,
        None,
        &default_with_members(),
    )
    .unwrap();
    assert_eq!(cfg.members.len(), 2);
    assert_eq!(cfg.strategy, ArbiterStrategyKind::Synthesize);
    assert!(cfg.quorum.is_none());
    assert!(cfg.max_cost_usd.is_none());
}

#[test]
fn resolve_extras_members_override_defaults() {
    let extras = PanelExtras {
        members: vec![
            "model-a".to_string(),
            "model-b".to_string(),
            "model-c".to_string(),
        ],
        arbiter_model: None,
        quorum: None,
        max_cost_usd: None,
    };
    let cfg = PanelConfig::resolve(
        ArbiterStrategyKind::BestOfN,
        Some(&extras),
        &default_with_members(),
    )
    .unwrap();
    assert_eq!(cfg.members.len(), 3);
    assert_eq!(cfg.members[0].model, "model-a");
}

#[test]
fn resolve_empty_members_after_merge_is_error() {
    let result = PanelConfig::resolve(ArbiterStrategyKind::Synthesize, None, &empty_defaults());
    assert!(
        matches!(result, Err(ApiError::InvalidRequest(_))),
        "expected InvalidRequest, got {result:?}"
    );
}

#[test]
fn resolve_extras_quorum_and_cost_propagate() {
    let extras = PanelExtras {
        members: vec!["m1".to_string(), "m2".to_string()],
        arbiter_model: Some("judge".to_string()),
        quorum: Some(2),
        max_cost_usd: Some(0.05),
    };
    let cfg = PanelConfig::resolve(
        ArbiterStrategyKind::Majority,
        Some(&extras),
        &default_with_members(),
    )
    .unwrap();
    assert_eq!(cfg.quorum, Some(2));
    assert_eq!(cfg.max_cost_usd, Some(0.05));
    assert_eq!(cfg.arbiter_model.model, "judge");
}

// ---------------------------------------------------------------------------
// tt_extras.panel parsing tests (Step 5)
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use tt_shared::messages::parse_panel_extras;

#[test]
fn parse_panel_extras_members_parsed() {
    let mut extras: HashMap<String, serde_json::Value> = HashMap::new();
    extras.insert(
        "panel".to_string(),
        serde_json::json!({ "members": ["model-a", "model-b"] }),
    );
    let panel = parse_panel_extras(&extras).unwrap();
    assert_eq!(panel.members, vec!["model-a", "model-b"]);
}

#[test]
fn parse_panel_extras_absent_is_none() {
    let extras: HashMap<String, serde_json::Value> = HashMap::new();
    assert!(parse_panel_extras(&extras).is_none());
}

// ---------------------------------------------------------------------------
// Member-count cap tests (TT_PANEL_MAX_MEMBERS, default 8)
// ---------------------------------------------------------------------------

/// Build a PanelDefaults with a single arbiter and no default members.
fn arbiter_only_defaults() -> PanelDefaults {
    PanelDefaults {
        members: vec![],
        arbiter_model: ModelRef {
            model: "gpt-4o".to_string(),
            provider: None,
        },
    }
}

/// Construct a PanelExtras with `n` synthetic member models.
fn extras_with_n_members(n: usize) -> PanelExtras {
    PanelExtras {
        members: (0..n).map(|i| format!("model-{i}")).collect(),
        arbiter_model: Some("gpt-4o".to_string()),
        quorum: None,
        max_cost_usd: None,
    }
}

#[test]
fn resolve_exactly_cap_members_is_ok() {
    // Default cap is 8; a request with exactly 8 members must succeed.
    let extras = extras_with_n_members(8);
    let result = PanelConfig::resolve(
        ArbiterStrategyKind::Synthesize,
        Some(&extras),
        &arbiter_only_defaults(),
    );
    assert!(
        result.is_ok(),
        "expected Ok for exactly 8 members, got {result:?}"
    );
    assert_eq!(result.unwrap().members.len(), 8);
}

#[test]
fn resolve_over_cap_members_is_invalid_request() {
    // Default cap is 8; a request with 9 members must be rejected.
    let extras = extras_with_n_members(9);
    let result = PanelConfig::resolve(
        ArbiterStrategyKind::Synthesize,
        Some(&extras),
        &arbiter_only_defaults(),
    );
    match result {
        Err(ApiError::InvalidRequest(msg)) => {
            assert!(
                msg.contains("9"),
                "error message should mention the count (9): {msg}"
            );
            assert!(
                msg.contains("8") || msg.contains("maximum"),
                "error message should mention the maximum: {msg}"
            );
        }
        other => panic!("expected Err(InvalidRequest), got {other:?}"),
    }
}
