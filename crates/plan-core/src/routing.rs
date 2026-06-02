//! Route matching. Given a request and a slice of proposed routes (sorted
//! by priority descending), return the first enabled route whose
//! conditions all match. Pure function — no state, no side effects.

use crate::types::{ProposedRoute, RequestLog, RouteConditions};

/// Match a request against routes in priority order. First match wins.
///
/// Routes are assumed already sorted by `priority` descending. Disabled
/// routes are skipped.
#[must_use]
pub fn match_route<'a>(req: &RequestLog, routes: &'a [ProposedRoute]) -> Option<&'a ProposedRoute> {
    routes
        .iter()
        .find(|r| r.enabled && matches_conditions(req, &r.when))
}

fn matches_conditions(req: &RequestLog, c: &RouteConditions) -> bool {
    if !c.model_in.is_empty() && !c.model_in.iter().any(|m| m == &req.model) {
        return false;
    }
    if let Some(t) = c.input_tokens_lt {
        if req.input_tokens >= t {
            return false;
        }
    }
    if let Some(t) = c.input_tokens_gt {
        if req.input_tokens <= t {
            return false;
        }
    }
    if let Some(tag) = &c.tag_equals {
        if req.tag.as_deref() != Some(tag.as_str()) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RouteAction;
    use chrono::Utc;
    use uuid::Uuid;

    fn req(model: &str, input_tokens: u32, tag: Option<&str>) -> RequestLog {
        RequestLog {
            id: Uuid::nil(),
            org_id: Uuid::nil(),
            ts: Utc::now(),
            provider: "anthropic".into(),
            model: model.into(),
            input_tokens,
            output_tokens: 0,
            cached_tokens: 0,
            cost_usd: 0.0,
            baseline_cost_usd: 0.0,
            cached: false,
            cache_layer: None,
            matched_route_id: None,
            latency_ms: 0,
            upstream_latency_ms: None,
            status: 200,
            tag: tag.map(String::from),
            embedding: None,
            finish_reason: None,
            body: None,
            response_body: None,
        }
    }

    fn route(name: &str, priority: u32, enabled: bool, when: RouteConditions) -> ProposedRoute {
        ProposedRoute {
            id: Uuid::new_v4(),
            name: name.into(),
            priority,
            enabled,
            when,
            then: RouteAction {
                target_model: "x".into(),
                force_cache_layer: None,
                fallbacks: Vec::new(),
            },
        }
    }

    #[test]
    fn empty_routes_no_match() {
        let r = req("m", 100, None);
        assert!(match_route(&r, &[]).is_none());
    }

    #[test]
    fn first_priority_wins() {
        let high = route(
            "high",
            100,
            true,
            RouteConditions {
                model_in: vec!["m".into()],
                ..Default::default()
            },
        );
        let low = route(
            "low",
            10,
            true,
            RouteConditions {
                model_in: vec!["m".into()],
                ..Default::default()
            },
        );
        let routes = vec![high.clone(), low];
        let r = req("m", 50, None);
        let m = match_route(&r, &routes).expect("a route should match");
        assert_eq!(m.name, "high");
    }

    #[test]
    fn disabled_skipped() {
        let r1 = route(
            "a",
            10,
            false,
            RouteConditions {
                model_in: vec!["m".into()],
                ..Default::default()
            },
        );
        let r2 = route(
            "b",
            5,
            true,
            RouteConditions {
                model_in: vec!["m".into()],
                ..Default::default()
            },
        );
        let r = req("m", 1, None);
        let routes = [r1, r2];
        let m = match_route(&r, &routes).expect("must fall through to enabled route");
        assert_eq!(m.name, "b");
    }

    #[test]
    fn token_bounds_inclusive_exclusion() {
        let r = route(
            "small",
            10,
            true,
            RouteConditions {
                input_tokens_lt: Some(200),
                ..Default::default()
            },
        );
        let routes = [r];
        // 199 matches.
        assert!(match_route(&req("m", 199, None), &routes).is_some());
        // 200 does not (strict less-than).
        assert!(match_route(&req("m", 200, None), &routes).is_none());
    }

    #[test]
    fn tag_equals_filter() {
        let r = route(
            "ux",
            10,
            true,
            RouteConditions {
                tag_equals: Some("ux".into()),
                ..Default::default()
            },
        );
        let routes = [r];
        assert!(match_route(&req("m", 1, Some("ux")), &routes).is_some());
        assert!(match_route(&req("m", 1, Some("api")), &routes).is_none());
        assert!(match_route(&req("m", 1, None), &routes).is_none());
    }

    #[test]
    fn all_conditions_anded() {
        let r = route(
            "and",
            10,
            true,
            RouteConditions {
                model_in: vec!["m".into()],
                input_tokens_lt: Some(100),
                tag_equals: Some("ux".into()),
                ..Default::default()
            },
        );
        let routes = [r];
        assert!(match_route(&req("m", 50, Some("ux")), &routes).is_some());
        assert!(match_route(&req("m", 50, Some("api")), &routes).is_none());
        assert!(match_route(&req("x", 50, Some("ux")), &routes).is_none());
        assert!(match_route(&req("m", 150, Some("ux")), &routes).is_none());
    }
}
