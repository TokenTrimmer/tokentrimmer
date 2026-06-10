//! Batch-eligibility advisor (Batch/Flex phase 1 — ADVISORY only).
//!
//! The OpenAI / Anthropic / Gemini Batch APIs price asynchronous (≤24h) traffic
//! at ~50% of standard — the single biggest no-quality-loss cost lever. Building
//! the durable batch-submission queue is deferred (P3); phase 1 is purely
//! advisory: detect request-log traffic that is **batch-eligible** (tagged
//! background / offline / nightly / bulk, i.e. latency-insensitive) and PROJECT
//! the savings of moving it to the Batch API.
//!
//! This module is the pure, tool-groundable core: given request-log aggregates
//! (which the advisor already reasons over) and the embedded pricing catalog, it
//! produces a [`BatchFinding`] per eligible tag segment with the eligible spend
//! and the projected Batch-API cost/savings. The savings are computed from the
//! **real per-model batch rates in the catalog** (`pricing.toml` carries
//! `batch_{input,output}_per_million`), NOT a hardcoded 50% — a model with no
//! catalog batch tier contributes no projected savings (conservative).
//!
//! Nothing here submits anything to a batch API; it only surfaces the projection.

use serde::{Deserialize, Serialize};

use crate::pricing::PricingCatalog;

/// Default set of tags treated as **non-interactive** (batch-eligible) traffic.
///
/// These mark latency-insensitive bulk / offline work that can tolerate the
/// Batch API's async (≤24h) turnaround. The set is overridable per call (see
/// [`project_batch_savings_with_tags`]) so a deployment can configure its own
/// "background" tag vocabulary; `tag=background` is the canonical example used
/// across the codebase (routing, request-log attribution).
pub const DEFAULT_BATCH_ELIGIBLE_TAGS: &[&str] =
    &["background", "offline", "nightly", "batch", "bulk", "async"];

/// One tag-grouped request-log aggregate — the condensed view the advisor /
/// inspect path computes over `request_logs` (e.g. `SELECT provider, model, tag,
/// SUM(input_tokens), SUM(output_tokens), SUM(cost_usd), COUNT(*) ... GROUP BY
/// provider, model, tag`). One row per `(provider, model, tag)` segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestAggregate {
    /// Registry provider id the requests were served by (e.g. `"openai"`).
    pub provider: String,
    /// Provider-side model id (e.g. `"gpt-5.5"`).
    pub model: String,
    /// The request tag for this segment. `None` = untagged traffic (never
    /// batch-eligible — an untagged request is assumed interactive).
    pub tag: Option<String>,
    /// Summed input tokens across the segment.
    pub input_tokens: u64,
    /// Summed output tokens across the segment.
    pub output_tokens: u64,
    /// Summed cost (USD) the org actually paid for the segment — the
    /// denominator for "% of spend".
    pub cost_usd: f64,
    /// Number of requests in the segment (for the human-readable summary).
    pub request_count: u64,
}

/// A projected-savings finding for one batch-eligible tag segment.
///
/// Produced only for segments whose tag is in the configured eligible set AND
/// whose `(provider, model)` carries a batch rate in the catalog. The projection
/// is `eligible_spend − projected_batch_cost`, both computed from the catalog's
/// real per-model batch rates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchFinding {
    /// The eligible tag this finding is for (e.g. `"nightly-evals"`).
    pub tag: String,
    /// Current spend (USD) attributable to this eligible segment — summed
    /// across every `(provider, model)` under the tag that has a catalog batch
    /// rate (segments with no batch tier are excluded, since they can't be
    /// projected and aren't batch-eligible at the provider).
    pub eligible_spend_usd: f64,
    /// Projected cost (USD) of the same traffic priced at the catalog's Batch
    /// API rates.
    pub projected_batch_cost_usd: f64,
    /// `eligible_spend_usd − projected_batch_cost_usd`, floored at 0. The
    /// advisory headline ("≈ $X/mo saved at the Batch rate").
    pub projected_savings_usd: f64,
    /// `eligible_spend_usd / total_spend_usd * 100` — what fraction of *all*
    /// spend (across every segment considered) this eligible tag represents.
    /// Drives the "tag=X is N% of spend and batch-eligible" phrasing.
    pub share_of_spend_pct: f64,
    /// Number of requests folded into this finding.
    pub request_count: u64,
}

impl BatchFinding {
    /// The effective batch discount this finding realizes, as a percentage of
    /// the eligible spend (`savings / eligible_spend * 100`). ~50 for providers
    /// at the documented Batch-API rate; lower if the segment mixes models with
    /// and without a batch tier. `0.0` when there is no eligible spend.
    #[must_use]
    pub fn discount_pct(&self) -> f64 {
        if self.eligible_spend_usd <= 0.0 {
            0.0
        } else {
            self.projected_savings_usd / self.eligible_spend_usd * 100.0
        }
    }

    /// A one-line, tool-grounded advisory sentence, e.g.
    /// `"tag=nightly-evals is 31.0% of spend and batch-eligible → ~$12.40/mo
    /// saved at the Batch API rate (−50%)"`.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "tag={} is {:.1}% of spend and batch-eligible → ~${:.2} saved at the Batch API rate (−{:.0}%) on {} request(s)",
            self.tag,
            self.share_of_spend_pct,
            self.projected_savings_usd,
            self.discount_pct(),
            self.request_count,
        )
    }
}

/// Project Batch-API savings over request-log aggregates using the default
/// non-interactive tag set ([`DEFAULT_BATCH_ELIGIBLE_TAGS`]).
///
/// See [`project_batch_savings_with_tags`] for the full contract.
#[must_use]
pub fn project_batch_savings(
    aggregates: &[RequestAggregate],
    catalog: &PricingCatalog,
) -> Vec<BatchFinding> {
    project_batch_savings_with_tags(aggregates, catalog, DEFAULT_BATCH_ELIGIBLE_TAGS)
}

/// Project Batch-API savings over request-log aggregates, treating any segment
/// whose tag (case-insensitive) is in `eligible_tags` as batch-eligible.
///
/// For each eligible segment, the catalog's per-model batch rates price the
/// segment's input + output tokens; the projected savings is
/// `current_spend − projected_batch_cost`. Segments are folded into one
/// [`BatchFinding`] per tag (a tag may span several models). Findings are
/// returned **descending by projected savings** so the biggest lever is first;
/// ties break by tag name for determinism.
///
/// A segment is **excluded** (contributes nothing, not flagged) when:
/// - its tag is `None` or not in `eligible_tags` (interactive / unknown
///   traffic is never batch-eligible), or
/// - its `(provider, model)` has no batch rate in the catalog (the provider has
///   no batch tier for that model, so there is nothing real to project — we do
///   NOT fabricate a 50% discount where catalog data is absent).
///
/// `share_of_spend_pct` is measured against the **total** spend of every
/// aggregate passed in (eligible or not), so it answers "what fraction of all
/// spend is this batch-eligible tag".
///
/// Tags that are eligible but whose entire spend is unpriceable produce no
/// finding (zero eligible spend → nothing to advise).
#[must_use]
pub fn project_batch_savings_with_tags(
    aggregates: &[RequestAggregate],
    catalog: &PricingCatalog,
    eligible_tags: &[&str],
) -> Vec<BatchFinding> {
    let total_spend: f64 = aggregates.iter().map(|a| a.cost_usd).sum();

    // Accumulate per-tag totals across all of the tag's batch-priceable models.
    // Keyed by the original tag string (case preserved for the summary).
    let mut by_tag: std::collections::BTreeMap<String, TagAccumulator> =
        std::collections::BTreeMap::new();

    for agg in aggregates {
        let Some(tag) = agg.tag.as_deref() else {
            continue; // untagged → interactive, never batch-eligible
        };
        if !eligible_tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
            continue; // tag not in the non-interactive set
        }
        // Resolve the catalog's batch rate for this model; skip if absent.
        let Some(pricing) = catalog.latest(&agg.provider, &agg.model) else {
            continue;
        };
        let (Some(batch_in), Some(batch_out)) = (
            pricing.batch_input_per_million,
            pricing.batch_output_per_million,
        ) else {
            continue; // no batch tier for this model → nothing to project
        };

        let projected = (agg.input_tokens as f64) * batch_in / 1_000_000.0
            + (agg.output_tokens as f64) * batch_out / 1_000_000.0;

        let entry = by_tag.entry(tag.to_string()).or_default();
        entry.eligible_spend += agg.cost_usd;
        entry.projected_batch += projected;
        entry.request_count += agg.request_count;
    }

    let mut findings: Vec<BatchFinding> = by_tag
        .into_iter()
        .filter(|(_, acc)| acc.eligible_spend > 0.0)
        .map(|(tag, acc)| {
            let savings = (acc.eligible_spend - acc.projected_batch).max(0.0);
            let share = if total_spend > 0.0 {
                acc.eligible_spend / total_spend * 100.0
            } else {
                0.0
            };
            BatchFinding {
                tag,
                eligible_spend_usd: acc.eligible_spend,
                projected_batch_cost_usd: acc.projected_batch,
                projected_savings_usd: savings,
                share_of_spend_pct: share,
                request_count: acc.request_count,
            }
        })
        .collect();

    // Biggest lever first; deterministic tie-break by tag.
    findings.sort_by(|a, b| {
        b.projected_savings_usd
            .partial_cmp(&a.projected_savings_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.tag.cmp(&b.tag))
    });
    findings
}

/// Running per-tag totals while folding aggregates.
#[derive(Default)]
struct TagAccumulator {
    eligible_spend: f64,
    projected_batch: f64,
    request_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::catalog;

    fn agg(
        provider: &str,
        model: &str,
        tag: Option<&str>,
        input: u64,
        output: u64,
        cost: f64,
        count: u64,
    ) -> RequestAggregate {
        RequestAggregate {
            provider: provider.into(),
            model: model.into(),
            tag: tag.map(str::to_string),
            input_tokens: input,
            output_tokens: output,
            cost_usd: cost,
            request_count: count,
        }
    }

    /// Core TDD behavior: a batch-eligible tagged segment produces a finding
    /// with the correct projected −50% (catalog-rate) savings + the eligible
    /// spend; non-eligible traffic in the same set is NOT flagged.
    #[test]
    fn flags_eligible_segment_with_catalog_rate_savings() {
        let c = catalog();
        // gpt-5.5: standard $5/$30 per 1M, batch $2.50/$15 per 1M (50% off).
        // 1M input + 1M output @ standard = $5 + $30 = $35 actual spend.
        // Plus an interactive (untagged) gpt-5.5 segment that must be ignored.
        let aggs = vec![
            agg(
                "openai",
                "gpt-5.5",
                Some("nightly"),
                1_000_000,
                1_000_000,
                35.0,
                10,
            ),
            agg("openai", "gpt-5.5", None, 1_000_000, 1_000_000, 35.0, 10),
        ];
        let findings = project_batch_savings(&aggs, c);
        assert_eq!(findings.len(), 1, "only the tagged segment is flagged");
        let f = &findings[0];
        assert_eq!(f.tag, "nightly");
        assert!(
            (f.eligible_spend_usd - 35.0).abs() < 1e-9,
            "eligible spend = actual spend of the tagged segment"
        );
        // batch cost: 1M*$2.50 + 1M*$15 = $17.50 → savings = $35 - $17.50.
        assert!(
            (f.projected_batch_cost_usd - 17.50).abs() < 1e-9,
            "batch cost from catalog rates, got {}",
            f.projected_batch_cost_usd
        );
        assert!(
            (f.projected_savings_usd - 17.50).abs() < 1e-9,
            "−50% of $35 = $17.50, got {}",
            f.projected_savings_usd
        );
        // share of spend: $35 eligible / $70 total = 50%.
        assert!((f.share_of_spend_pct - 50.0).abs() < 1e-9);
        // discount is the real catalog 50%, not a hardcoded constant.
        assert!((f.discount_pct() - 50.0).abs() < 1e-9);
        assert_eq!(f.request_count, 10);
    }

    /// Non-eligible tags (an interactive tag like "chat") are never flagged,
    /// even when traffic exists for them.
    #[test]
    fn ignores_non_eligible_tags() {
        let c = catalog();
        let aggs = vec![
            agg(
                "openai",
                "gpt-5.5",
                Some("chat"),
                1_000_000,
                1_000_000,
                35.0,
                5,
            ),
            agg("openai", "gpt-5.5", Some("interactive"), 500_000, 0, 2.5, 3),
        ];
        let findings = project_batch_savings(&aggs, c);
        assert!(
            findings.is_empty(),
            "no eligible tags present → no findings: {findings:?}"
        );
    }

    /// A model with NO catalog batch tier contributes no projected savings —
    /// we never fabricate a 50% discount where the catalog has no data.
    #[test]
    fn model_without_batch_tier_is_not_projected() {
        let c = catalog();
        // groq llama has no batch_{input,output}_per_million in the catalog.
        let aggs = vec![agg(
            "groq",
            "llama-3.1-8b-instant",
            Some("background"),
            1_000_000,
            1_000_000,
            1.0,
            4,
        )];
        let findings = project_batch_savings(&aggs, c);
        assert!(
            findings.is_empty(),
            "no batch tier → nothing to project, got {findings:?}"
        );
    }

    /// One eligible tag spanning several models folds into a single finding,
    /// summing eligible spend and per-model batch projections.
    #[test]
    fn folds_multiple_models_under_one_tag() {
        let c = catalog();
        // gpt-5.5 batch $2.50/$15 and gpt-5.4 batch $1.25/$7.50.
        let aggs = vec![
            agg("openai", "gpt-5.5", Some("bulk"), 1_000_000, 0, 5.0, 2),
            agg("openai", "gpt-5.4", Some("bulk"), 1_000_000, 0, 2.5, 3),
        ];
        let findings = project_batch_savings(&aggs, c);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.tag, "bulk");
        assert!((f.eligible_spend_usd - 7.5).abs() < 1e-9, "5.0 + 2.5");
        // batch input: 1M*$2.50 + 1M*$1.25 = $3.75 → savings $7.5 - $3.75.
        assert!((f.projected_batch_cost_usd - 3.75).abs() < 1e-9);
        assert!((f.projected_savings_usd - 3.75).abs() < 1e-9);
        assert_eq!(f.request_count, 5);
    }

    /// Findings sort by projected savings (biggest lever first), tie-break tag.
    #[test]
    fn findings_sorted_by_savings_desc() {
        let c = catalog();
        let aggs = vec![
            // small eligible segment
            agg("openai", "gpt-5.4", Some("offline"), 1_000_000, 0, 2.5, 1),
            // large eligible segment
            agg("openai", "gpt-5.5", Some("nightly"), 10_000_000, 0, 50.0, 1),
        ];
        let findings = project_batch_savings(&aggs, c);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].tag, "nightly", "bigger savings first");
        assert!(findings[0].projected_savings_usd > findings[1].projected_savings_usd);
    }

    /// Tag matching is case-insensitive ("Background" matches "background").
    #[test]
    fn tag_match_is_case_insensitive() {
        let c = catalog();
        let aggs = vec![agg(
            "openai",
            "gpt-5.5",
            Some("Background"),
            1_000_000,
            0,
            5.0,
            1,
        )];
        let findings = project_batch_savings(&aggs, c);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].tag, "Background", "original case preserved");
    }

    /// A custom (configurable) eligible-tag set is honored; the default
    /// vocabulary is ignored when an explicit set is given.
    #[test]
    fn honors_configurable_tag_set() {
        let c = catalog();
        let aggs = vec![
            agg(
                "openai",
                "gpt-5.5",
                Some("nightly-evals"),
                1_000_000,
                0,
                5.0,
                1,
            ),
            // "background" is in the DEFAULT set but NOT in our custom set:
            agg(
                "openai",
                "gpt-5.5",
                Some("background"),
                1_000_000,
                0,
                5.0,
                1,
            ),
        ];
        let findings = project_batch_savings_with_tags(&aggs, c, &["nightly-evals"]);
        assert_eq!(findings.len(), 1, "only the custom tag matches");
        assert_eq!(findings[0].tag, "nightly-evals");
    }

    /// The summary sentence is tool-grounded: carries the tag, the share of
    /// spend, the dollar savings, and the realized discount.
    #[test]
    fn summary_is_grounded_and_human_readable() {
        let f = BatchFinding {
            tag: "nightly-evals".into(),
            eligible_spend_usd: 40.0,
            projected_batch_cost_usd: 20.0,
            projected_savings_usd: 20.0,
            share_of_spend_pct: 31.0,
            request_count: 128,
        };
        let s = f.summary();
        assert!(s.contains("tag=nightly-evals"), "{s}");
        assert!(s.contains("31.0% of spend"), "{s}");
        assert!(s.contains("$20.00"), "{s}");
        assert!(s.contains("−50%"), "{s}");
        assert!(s.contains("128 request"), "{s}");
    }

    /// Empty input → no findings, no panic (e.g. division by zero on share).
    #[test]
    fn empty_aggregates_produce_no_findings() {
        let c = catalog();
        assert!(project_batch_savings(&[], c).is_empty());
    }

    /// Anthropic batch rates also flow through (50% per the catalog).
    #[test]
    fn anthropic_eligible_segment_uses_catalog_batch_rate() {
        let c = catalog();
        // claude-opus-4-8: standard $5/$25, batch $2.50/$12.50.
        // 1M in + 1M out standard = $30 actual.
        let aggs = vec![agg(
            "anthropic",
            "claude-opus-4-8",
            Some("offline"),
            1_000_000,
            1_000_000,
            30.0,
            7,
        )];
        let findings = project_batch_savings(&aggs, c);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        // batch: 1M*$2.50 + 1M*$12.50 = $15 → savings $30 - $15 = $15.
        assert!((f.projected_batch_cost_usd - 15.0).abs() < 1e-9);
        assert!((f.projected_savings_usd - 15.0).abs() < 1e-9);
    }
}
