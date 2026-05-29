//! L1 cache hit projection. v1 ships exact-match keyed on
//! `(provider, model, input_tokens, tag)` — a coarse stand-in for the
//! "normalized request shape" hash described in
//! `docs/03-plan-replay-design.md` §6.1.
//!
//! L2 (semantic) projection requires per-request embeddings (ADR-008) and
//! lands in a follow-up; the [`CacheProjection`] struct is shaped to grow
//! into that without a breaking change.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::types::{CacheProjection, PlanConfig, RequestLog};

/// Project L1 cache hits over a request window under the proposed config.
///
/// Algorithm: iterate `requests` in timestamp ascending order. For each
/// request, compute a coarse normalized key. If we've seen the same key
/// within the configured TTL, count it as a hit; otherwise record the
/// timestamp as the new cache-population time for that key.
///
/// Returns an all-zero projection when `config.l1_ttl_seconds` is `None`
/// (caller opted out of L1 projection entirely).
#[must_use]
pub fn project_l1_hits(requests: &[RequestLog], config: &PlanConfig) -> CacheProjection {
    let total = requests.len() as u32;
    let Some(ttl_secs) = config.l1_ttl_seconds else {
        return CacheProjection {
            total,
            projected_l1_hits: 0,
            projected_l1_hit_rate: 0.0,
        };
    };
    if requests.is_empty() {
        return CacheProjection::default();
    }
    let ttl = chrono::Duration::seconds(i64::from(ttl_secs));

    // Stable order by (ts, id) so determinism is preserved across input
    // permutations.
    let mut sorted: Vec<&RequestLog> = requests.iter().collect();
    sorted.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));

    let mut last_seen: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut hits: u32 = 0;
    for req in sorted {
        let key = normalized_key(req);
        if let Some(prev_ts) = last_seen.get(&key) {
            if req.ts.signed_duration_since(*prev_ts) <= ttl {
                hits += 1;
                // Hits don't reset the cache entry — the original miss
                // populated it. Skip the insert.
                continue;
            }
        }
        last_seen.insert(key, req.ts);
    }

    let rate = if total == 0 {
        0.0
    } else {
        f64::from(hits) / f64::from(total)
    };
    CacheProjection {
        total,
        projected_l1_hits: hits,
        projected_l1_hit_rate: rate,
    }
}

/// Coarse normalized cache key. Exact-match L1 in production hashes the
/// full normalized request body; in replay we only have telemetry, so the
/// best we can do is hash the recorded request shape. Good enough for the
/// "would this proposed cache TTL have helped?" question.
fn normalized_key(req: &RequestLog) -> String {
    format!(
        "{}|{}|{}|{}",
        req.provider,
        req.model,
        req.input_tokens,
        req.tag.as_deref().unwrap_or("")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn make_req(secs: i64, model: &str, tokens: u32, tag: Option<&str>) -> RequestLog {
        RequestLog {
            id: Uuid::new_v4(),
            org_id: Uuid::nil(),
            ts: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap()
                + chrono::Duration::seconds(secs),
            provider: "anthropic".into(),
            model: model.into(),
            input_tokens: tokens,
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

    #[test]
    fn no_ttl_means_zero_hits() {
        let reqs = vec![make_req(0, "m", 100, None), make_req(1, "m", 100, None)];
        let cfg = PlanConfig::default();
        let p = project_l1_hits(&reqs, &cfg);
        assert_eq!(p.total, 2);
        assert_eq!(p.projected_l1_hits, 0);
    }

    #[test]
    fn second_identical_request_within_ttl_is_a_hit() {
        let reqs = vec![make_req(0, "m", 100, None), make_req(30, "m", 100, None)];
        let cfg = PlanConfig {
            l1_ttl_seconds: Some(60),
            ..PlanConfig::default()
        };
        let p = project_l1_hits(&reqs, &cfg);
        assert_eq!(p.projected_l1_hits, 1);
        assert!((p.projected_l1_hit_rate - 0.5).abs() < 1e-12);
    }

    #[test]
    fn request_outside_ttl_is_a_miss() {
        let reqs = vec![make_req(0, "m", 100, None), make_req(120, "m", 100, None)];
        let cfg = PlanConfig {
            l1_ttl_seconds: Some(60),
            ..PlanConfig::default()
        };
        let p = project_l1_hits(&reqs, &cfg);
        assert_eq!(p.projected_l1_hits, 0);
    }

    #[test]
    fn different_shape_no_hit() {
        let reqs = vec![make_req(0, "m", 100, None), make_req(1, "m", 101, None)];
        let cfg = PlanConfig {
            l1_ttl_seconds: Some(60),
            ..PlanConfig::default()
        };
        let p = project_l1_hits(&reqs, &cfg);
        assert_eq!(p.projected_l1_hits, 0);
    }
}
