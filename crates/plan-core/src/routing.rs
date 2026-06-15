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
    // Cost is a logged field, so cost conditions project accurately (no caveat).
    if let Some(t) = c.estimated_cost_gt {
        if req.baseline_cost_usd <= t {
            return false;
        }
    }
    if let Some(t) = c.estimated_cost_lt {
        if req.baseline_cost_usd >= t {
            return false;
        }
    }
    if let Some(tag) = &c.tag_equals {
        if req.tag.as_deref() != Some(tag.as_str()) {
            return false;
        }
    }
    // Modality conditions cannot be evaluated against historical RequestLog rows
    // (no modality recorded). Treat ANY modality requirement as a non-match so
    // Plan never over-projects savings. Follow-up: capture had_images/had_audio
    // on request_logs to enable modality projection.
    if c.has_images.is_some() || c.has_audio.is_some() {
        return false;
    }
    // The live p95-latency condition is backed by the gateway's in-process
    // rolling window, which does not exist for historical rows. Like modality,
    // a route carrying it is a conservative non-match in replay — Plan never
    // over-projects savings on a latency-gated route.
    if c.upstream_latency_ms_p95_gt.is_some() {
        return false;
    }
    if !c.prompt_contains_any_of.is_empty() {
        let Some(body) = &req.body else {
            return false;
        };
        let text = body.to_lowercase();
        if !c
            .prompt_contains_any_of
            .iter()
            .any(|kw| text.contains(&kw.to_lowercase()))
        {
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
            task_class: Default::default(),
            diff_saved_usd: None,
            minify_saved_est_usd: None,
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
                format_switch: None,
                diff: false,
                target_model: Some("x".into()),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
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

    #[test]
    fn modality_condition_never_matches_historical_log() {
        // RequestLog carries no modality, so a modality-conditioned route must
        // not match — Plan stays conservative and never over-projects savings.
        let r = route(
            "img-only",
            10,
            true,
            RouteConditions {
                has_images: Some(true),
                ..Default::default()
            },
        );
        assert!(match_route(&req("m", 1, None), &[r]).is_none());

        let r2 = route(
            "no-img",
            10,
            true,
            RouteConditions {
                has_images: Some(false),
                ..Default::default()
            },
        );
        assert!(match_route(&req("m", 1, None), &[r2]).is_none());
    }

    #[test]
    fn prompt_contains_matches_body_else_no_match() {
        let r = route(
            "topic",
            10,
            true,
            RouteConditions {
                prompt_contains_any_of: vec!["confidential".into()],
                ..Default::default()
            },
        );
        // No body → conservative no-match.
        assert!(match_route(&req("m", 1, None), &[r.clone()]).is_none());
        // Body containing the keyword (case-insensitive) → match.
        let mut with_body = req("m", 1, None);
        with_body.body = Some("This is CONFIDENTIAL".into());
        assert!(match_route(&with_body, &[r]).is_some());
    }

    #[test]
    fn cost_gt_matches_on_baseline_cost() {
        // Unlike modality/topic, cost IS logged — evaluate against baseline_cost_usd.
        let r = route(
            "expensive",
            10,
            true,
            RouteConditions {
                estimated_cost_gt: Some(0.02),
                ..Default::default()
            },
        );
        let mut hi = req("m", 100, None);
        hi.baseline_cost_usd = 0.03;
        let mut lo = req("m", 100, None);
        lo.baseline_cost_usd = 0.01;
        assert!(match_route(&hi, std::slice::from_ref(&r)).is_some());
        assert!(match_route(&lo, &[r]).is_none());
    }

    #[test]
    fn cost_lt_matches_below_threshold() {
        let r = route(
            "cheap",
            10,
            true,
            RouteConditions {
                estimated_cost_lt: Some(0.02),
                ..Default::default()
            },
        );
        let mut lo = req("m", 100, None);
        lo.baseline_cost_usd = 0.01;
        let mut hi = req("m", 100, None);
        hi.baseline_cost_usd = 0.05;
        assert!(match_route(&lo, std::slice::from_ref(&r)).is_some());
        assert!(match_route(&hi, &[r]).is_none());
    }
}
