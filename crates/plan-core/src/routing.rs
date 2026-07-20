//! Route matching. Given a request and a slice of proposed routes (sorted
//! by priority descending), return the first enabled route whose
//! conditions all match. Pure function — no state, no side effects.

use crate::{
    error::PlanError,
    types::{ProposedRoute, RequestLog, RouteConditions},
};
use tt_routing::{route_conditions_match, RouteFeatureSnapshot};

pub(crate) struct PreparedRoute {
    route: ProposedRoute,
    conditions: tt_routing::RouteConditions,
    action: tt_routing::RouteAction,
}

impl PreparedRoute {
    pub(crate) fn route(&self) -> &ProposedRoute {
        &self.route
    }

    pub(crate) fn action(&self) -> &tt_routing::RouteAction {
        &self.action
    }
}

pub(crate) fn prepare_routes(routes: Vec<ProposedRoute>) -> Result<Vec<PreparedRoute>, PlanError> {
    routes
        .into_iter()
        .map(|route| {
            let encoded = serde_json::to_value(&route.when).map_err(|error| {
                PlanError::RouteConditionContract {
                    route_id: route.id,
                    message: error.to_string(),
                }
            })?;
            let conditions = serde_json::from_value(encoded).map_err(|error| {
                PlanError::RouteConditionContract {
                    route_id: route.id,
                    message: error.to_string(),
                }
            })?;
            let encoded = serde_json::to_value(&route.then).map_err(|error| {
                PlanError::RouteActionContract {
                    route_id: route.id,
                    message: error.to_string(),
                }
            })?;
            let action = serde_json::from_value(encoded).map_err(|error| {
                PlanError::RouteActionContract {
                    route_id: route.id,
                    message: error.to_string(),
                }
            })?;
            Ok(PreparedRoute {
                route,
                conditions,
                action,
            })
        })
        .collect()
}

/// Match a request against routes in priority order. First match wins.
///
/// Routes are assumed already sorted by `priority` descending. Disabled
/// routes are skipped.
#[must_use]
pub fn match_route<'a>(req: &RequestLog, routes: &'a [ProposedRoute]) -> Option<&'a ProposedRoute> {
    let snapshot = historical_snapshot(req);
    routes.iter().find(|route| {
        route.enabled
            && gateway_conditions(&route.when)
                .is_some_and(|conditions| route_conditions_match(&conditions, &snapshot))
    })
}

pub(crate) fn match_prepared_route<'a>(
    req: &RequestLog,
    routes: &'a [PreparedRoute],
) -> Option<&'a PreparedRoute> {
    let snapshot = historical_snapshot(req);
    routes.iter().find(|prepared| {
        let route = prepared.route();
        route.enabled && route_conditions_match(&prepared.conditions, &snapshot)
    })
}

fn historical_snapshot(req: &RequestLog) -> RouteFeatureSnapshot {
    let snapshot = RouteFeatureSnapshot::from_partial_retained_features(
        req.requested_model.clone(),
        req.input_tokens,
        req.tag.clone(),
    );
    if req.baseline_cost_usd.is_finite() && req.baseline_cost_usd >= 0.0 {
        snapshot.with_approximate_estimated_cost_usd(req.baseline_cost_usd)
    } else {
        snapshot
    }
}

fn gateway_conditions(c: &RouteConditions) -> Option<tt_routing::RouteConditions> {
    serde_json::to_value(c)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(crate) fn ambiguous_equal_priority_pair(
    routes: &[ProposedRoute],
) -> Option<(&ProposedRoute, &ProposedRoute)> {
    for (index, left) in routes.iter().enumerate() {
        if !left.enabled {
            continue;
        }
        for right in routes.iter().skip(index + 1) {
            if right.enabled
                && left.priority == right.priority
                && conditions_may_overlap(&left.when, &right.when)
            {
                return Some((left, right));
            }
        }
    }
    None
}

fn conditions_may_overlap(left: &RouteConditions, right: &RouteConditions) -> bool {
    !route_is_impossible(left)
        && !route_is_impossible(right)
        && !model_sets_are_disjoint(&left.model_in, &right.model_in)
        && !different_values(left.tag_equals.as_ref(), right.tag_equals.as_ref())
        && !different_values(left.has_images.as_ref(), right.has_images.as_ref())
        && !different_values(left.has_audio.as_ref(), right.has_audio.as_ref())
        && !different_values(left.has_documents.as_ref(), right.has_documents.as_ref())
        && !different_values(left.content_type.as_ref(), right.content_type.as_ref())
        && input_intervals_overlap(left, right)
        && cost_intervals_overlap(left, right)
}

fn route_is_impossible(conditions: &RouteConditions) -> bool {
    input_interval(conditions).is_none() || cost_interval(conditions).is_none()
}

fn model_sets_are_disjoint(left: &[String], right: &[String]) -> bool {
    !left.is_empty()
        && !right.is_empty()
        && !left
            .iter()
            .any(|model| right.iter().any(|candidate| candidate == model))
}

fn different_values<T: PartialEq>(left: Option<&T>, right: Option<&T>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}

fn input_interval(conditions: &RouteConditions) -> Option<(u64, u64)> {
    let lower = conditions
        .input_tokens_gt
        .map_or(0, |value| u64::from(value) + 1);
    let upper = match conditions.input_tokens_lt {
        Some(0) => return None,
        Some(value) => u64::from(value - 1),
        None => u64::from(u32::MAX),
    };
    (lower <= upper).then_some((lower, upper))
}

fn input_intervals_overlap(left: &RouteConditions, right: &RouteConditions) -> bool {
    match (input_interval(left), input_interval(right)) {
        (Some((left_min, left_max)), Some((right_min, right_max))) => {
            left_min.max(right_min) <= left_max.min(right_max)
        }
        _ => false,
    }
}

fn cost_interval(conditions: &RouteConditions) -> Option<(f64, f64)> {
    let lower = conditions.estimated_cost_gt.unwrap_or(0.0);
    let upper = conditions.estimated_cost_lt.unwrap_or(f64::INFINITY);
    (lower.is_finite() && !upper.is_nan() && lower < upper).then_some((lower, upper))
}

fn cost_intervals_overlap(left: &RouteConditions, right: &RouteConditions) -> bool {
    match (cost_interval(left), cost_interval(right)) {
        (Some((left_min, left_max)), Some((right_min, right_max))) => {
            left_min.max(right_min) < left_max.min(right_max)
        }
        _ => false,
    }
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
            requested_model: Some(model.into()),
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
                ..Default::default()
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
    fn prompt_contains_never_infers_matcher_text_from_quality_body() {
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
        // A quality-judge body is not proven to be the exact user/system text
        // observed by the live matcher, so it remains unavailable too.
        let mut with_body = req("m", 1, None);
        with_body.body = Some("This is CONFIDENTIAL".into());
        assert!(match_route(&with_body, &[r]).is_none());
    }

    #[test]
    fn model_condition_never_substitutes_final_served_model() {
        let route = route(
            "requested-only",
            10,
            true,
            RouteConditions {
                model_in: vec!["served-model".into()],
                ..Default::default()
            },
        );
        let mut legacy = req("served-model", 1, None);
        legacy.requested_model = None;

        assert!(match_route(&legacy, &[route]).is_none());
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
