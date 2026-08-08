//! Pure parity corpus for the canonical route-condition matcher against the
//! dashboard's safe-fragment query translation.
//!
//! The authoritative JSON lives in `docs/route-static-parity-contract/` so a
//! hosted dashboard can vendor the exact bytes paired with its pinned gateway
//! revision. The dashboard translates a route's condition JSON into an
//! independent set/equality/strict-bound constraint region
//! (`cloud/apps/dashboard/src/lib/route-static-analysis.ts`
//! `safeConditionRegion` + `conditionRegionsMayOverlap` +
//! `conditionRegionSubsumes`, with the `priority DESC, created_at ASC, id ASC`
//! store-order tie-break). This test proves that translation agrees with the
//! canonical matcher (`crate::matcher`) on:
//!
//!   * non-ASCII prompt text — the matcher lower-cases with full Unicode while
//!     the dashboard's ASCII-only safe fragment fails closed (never proves
//!     overlap it cannot reason about),
//!   * the closed 13-condition-field set, with decode/trim/replace semantics
//!     identical (one JSON decode replaces `\uXXXX` escapes before analysis;
//!     neither side trims; prompt keywords are lower-cased, not case-folded),
//!   * overlapping equal-priority conditions being flagged consistently with
//!     the engine's first-match-wins outcome under the persisted store order.
//!
//! The approved corpus is read-only; every expectation is encoded in the JSON.
//! Each test is pure and deterministic (no database, no clock, no I/O).

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::Value;
use tt_routing::{
    Route, RouteAction, RouteConditionField, RouteConditionOutcome, RouteConditions,
    RouteFeatureSnapshot, RoutingEngine, ROUTE_SCHEMA_ID, ROUTE_SCHEMA_VERSION,
};
use uuid::Uuid;

const CORPUS: &str = include_str!(
    "../../../docs/route-static-parity-contract/tokentrimmer.route-condition-parity.corpus.json"
);
const CORPUS_FORMAT_ID: &str = "tokentrimmer.route-condition-parity-corpus";
const CORPUS_FORMAT_VERSION: u32 = 1;

/// Mirrors `MAX_CANONICAL_ITEMS` / `MAX_CANONICAL_CHARS` / `U32_MAX` from
/// `route-static-analysis.ts` so the ported parser keeps identical caps.
const MAX_CANONICAL_ITEMS: usize = 128;
const MAX_CANONICAL_CHARS: usize = 16 * 1024;
const U32_MAX: u64 = 4_294_967_295;

// ---------------------------------------------------------------------------
// Corpus shapes (the corpus is the single source of truth).
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConditionFieldPin {
    field: String,
    #[serde(rename = "matcher")]
    _matcher: String,
    #[serde(rename = "dashboard")]
    _dashboard: String,
    #[serde(rename = "parity")]
    _parity: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptCase {
    id: String,
    keywords: Vec<String>,
    prompt: String,
    expected_match: bool,
    dashboard_region: Option<Value>,
    #[serde(default)]
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldCase {
    id: String,
    field: String,
    conditions: Value,
    features: Value,
    expected_outcome: String,
    expected_match: bool,
    dashboard_region: Option<Value>,
    #[serde(default)]
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FailClosedCase {
    id: String,
    conditions: Value,
    #[serde(rename = "expected")]
    _expected: String,
    #[serde(default)]
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlapRouteSpec {
    name: String,
    id: String,
    priority: u32,
    created_at: String,
    conditions: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlapRequest {
    id: String,
    features: Value,
    expected_winner: String,
    #[serde(default)]
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlapPair {
    candidate: String,
    subject: String,
    relation: String,
    expected: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlapCase {
    id: String,
    routes: Vec<OverlapRouteSpec>,
    requests: Vec<OverlapRequest>,
    pairs: Vec<OverlapPair>,
    #[serde(default)]
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParityCorpus {
    corpus: CorpusFormat,
    route_contract: RouteContractVersion,
    condition_fields: Vec<ConditionFieldPin>,
    prompt_cases: Vec<PromptCase>,
    field_cases: Vec<FieldCase>,
    fail_closed_cases: Vec<FailClosedCase>,
    overlap_cases: Vec<OverlapCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusFormat {
    id: String,
    version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteContractVersion {
    id: String,
    version: u32,
}

fn corpus() -> ParityCorpus {
    serde_json::from_str(CORPUS).expect("parity corpus must remain valid JSON")
}

// ---------------------------------------------------------------------------
// The dashboard's safe-fragment translation, ported exactly.
//
// These mirror `route-static-analysis.ts` semantics on already-decoded JSON
// values: `safeConditionRegion`, `conditionRegionsMayOverlap`, and
// `conditionRegionSubsumes`. Where the TS uses JS `toLowerCase()` over an
// ASCII-only admitted keyword set, the port uses `to_ascii_lowercase`, which
// is identical on ASCII. Returning `None` is "not proven", never "does not
// overlap" — the fail-closed contract.
// ---------------------------------------------------------------------------

const CLOSED_CONDITION_KEYS: [&str; 13] = [
    "model_in",
    "input_tokens_lt",
    "input_tokens_gt",
    "tag_equals",
    "has_images",
    "has_audio",
    "has_documents",
    "content_type",
    "prompt_contains_any_of",
    "estimated_cost_gt",
    "estimated_cost_lt",
    "upstream_latency_ms_p95_gt",
    "not_reasoning_class",
];

#[derive(Debug, Clone, PartialEq)]
struct DashboardSafeRegion {
    model_in: Option<BTreeSet<String>>,
    input_tokens_gt: Option<u32>,
    input_tokens_lt: Option<u32>,
    tag_equals: Option<String>,
    has_images: Option<bool>,
    has_audio: Option<bool>,
    has_documents: Option<bool>,
    content_type: Option<String>,
    prompt_keywords: Option<Vec<String>>,
    estimated_cost_gt: Option<f64>,
    estimated_cost_lt: Option<f64>,
    upstream_latency_ms_p95_gt: Option<u32>,
    not_reasoning_class: bool,
}

fn is_ascii(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii())
}

/// `optionalU32`: absent or null → Ok(None); present but not a u32-range
/// integer → Err (whole region fails closed).
fn optional_u32(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u32>, ()> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let number = value.as_f64().ok_or(())?;
            if number.fract() != 0.0 || number < 0.0 || number > U32_MAX as f64 {
                return Err(());
            }
            Ok(Some(number as u32))
        }
    }
}

fn optional_nonnegative_number(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<f64>, ()> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let number = value.as_f64().ok_or(())?;
            if number < 0.0 {
                return Err(());
            }
            Ok(Some(number))
        }
    }
}

fn optional_string(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>, ()> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(()),
    }
}

fn optional_bool(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<bool>, ()> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(()),
    }
}

/// `safeConditionRegion(value)`: the bounded, independent
/// set/equality/strict-bound constraint fragment. `None` means the dashboard
/// could not reason about this exact shape.
fn safe_condition_region(value: &Value) -> Option<DashboardSafeRegion> {
    let obj = value.as_object()?;
    if obj
        .keys()
        .any(|key| !CLOSED_CONDITION_KEYS.contains(&key.as_str()))
    {
        return None;
    }

    let mut model_in: Option<BTreeSet<String>> = None;
    if let Some(raw) = obj.get("model_in") {
        let models = raw.as_array()?;
        if models.len() > MAX_CANONICAL_ITEMS {
            return None;
        }
        if models.iter().any(|member| {
            !member
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= MAX_CANONICAL_CHARS)
        }) {
            return None;
        }
        if !models.is_empty() {
            model_in = Some(
                models
                    .iter()
                    .map(|member| member.as_str().expect("checked above").to_owned())
                    .collect(),
            );
        }
    }

    let mut prompt_keywords: Option<Vec<String>> = None;
    if let Some(raw) = obj.get("prompt_contains_any_of") {
        let keywords = raw.as_array()?;
        if keywords.len() > MAX_CANONICAL_ITEMS {
            return None;
        }
        // The dashboard admits only non-empty fully-ASCII keywords. The
        // runtime matcher lower-cases non-ASCII Unicode-wise (see the prompt
        // parity cases), so the safe fragment cannot reason about them and
        // fails closed by design.
        if keywords.iter().any(|keyword| {
            !keyword
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= MAX_CANONICAL_CHARS && is_ascii(s))
        }) {
            return None;
        }
        if !keywords.is_empty() {
            prompt_keywords = Some(
                keywords
                    .iter()
                    .map(|keyword| {
                        keyword
                            .as_str()
                            .expect("checked above")
                            .to_ascii_lowercase()
                    })
                    .collect(),
            );
        }
    }

    let input_tokens_gt = optional_u32(obj, "input_tokens_gt").ok()?;
    let input_tokens_lt = optional_u32(obj, "input_tokens_lt").ok()?;
    let tag_equals = optional_string(obj, "tag_equals").ok()?;
    let has_images = optional_bool(obj, "has_images").ok()?;
    let has_audio = optional_bool(obj, "has_audio").ok()?;
    let has_documents = optional_bool(obj, "has_documents").ok()?;
    let content_type = optional_string(obj, "content_type").ok()?;
    let estimated_cost_gt = optional_nonnegative_number(obj, "estimated_cost_gt").ok()?;
    let estimated_cost_lt = optional_nonnegative_number(obj, "estimated_cost_lt").ok()?;
    let upstream_latency_ms_p95_gt = optional_u32(obj, "upstream_latency_ms_p95_gt").ok()?;

    let mut not_reasoning_class = false;
    if let Some(raw) = obj.get("not_reasoning_class") {
        let b = raw.as_bool()?;
        not_reasoning_class = b;
    }

    // Strict u32 / real-valued intervals must retain at least one signal after
    // the documented reachability checks (identical to the dashboard's):
    //   * input/latency have no u32 value > U32_MAX or < 0,
    //   * `> gt` and `< lt` strict bounds share no integer/real.
    if input_tokens_gt == Some(U32_MAX as u32)
        || input_tokens_lt == Some(0)
        || (input_tokens_gt.is_some()
            && input_tokens_lt.is_some()
            && (input_tokens_gt.unwrap() as u64) + 1 >= input_tokens_lt.unwrap() as u64)
        || upstream_latency_ms_p95_gt == Some(U32_MAX as u32)
        || (estimated_cost_gt.is_some()
            && estimated_cost_lt.is_some()
            && estimated_cost_gt.unwrap() >= estimated_cost_lt.unwrap())
    {
        return None;
    }

    Some(DashboardSafeRegion {
        model_in,
        input_tokens_gt,
        input_tokens_lt,
        tag_equals,
        has_images,
        has_audio,
        has_documents,
        content_type,
        prompt_keywords,
        estimated_cost_gt,
        estimated_cost_lt,
        upstream_latency_ms_p95_gt,
        not_reasoning_class,
    })
}

/// `conditionRegionSubsumes`: whether every request constrained by `narrower`
/// is also admitted by `broader`.
fn region_subsumes(broader: &DashboardSafeRegion, narrower: &DashboardSafeRegion) -> bool {
    let set_subsumes = |b: &Option<BTreeSet<String>>, n: &Option<BTreeSet<String>>| match b {
        None => true,
        Some(broad) => match n {
            None => false,
            Some(narrow) => narrow.iter().all(|value| broad.contains(value)),
        },
    };
    let lower_subsumes =
        |b: Option<f64>, n: Option<f64>| b.is_none() || (n.is_some() && b.unwrap() <= n.unwrap());
    let upper_subsumes =
        |b: Option<f64>, n: Option<f64>| b.is_none() || (n.is_some() && b.unwrap() >= n.unwrap());
    let prompt_subsumes = |b: &Option<Vec<String>>, n: &Option<Vec<String>>| match b {
        None => true,
        Some(broad) => match n {
            None => false,
            Some(narrow) => narrow.iter().all(|narrow_keyword| {
                broad
                    .iter()
                    .any(|broad_keyword| narrow_keyword.contains(broad_keyword))
            }),
        },
    };

    set_subsumes(&broader.model_in, &narrower.model_in)
        && lower_subsumes(
            broader.input_tokens_gt.map(f64::from),
            narrower.input_tokens_gt.map(f64::from),
        )
        && upper_subsumes(
            broader.input_tokens_lt.map(f64::from),
            narrower.input_tokens_lt.map(f64::from),
        )
        && exact_option_subsumes(&broader.tag_equals, &narrower.tag_equals)
        && exact_option_subsumes(&broader.has_images, &narrower.has_images)
        && exact_option_subsumes(&broader.has_audio, &narrower.has_audio)
        && exact_option_subsumes(&broader.has_documents, &narrower.has_documents)
        && exact_option_subsumes(&broader.content_type, &narrower.content_type)
        && prompt_subsumes(&broader.prompt_keywords, &narrower.prompt_keywords)
        && lower_subsumes(broader.estimated_cost_gt, narrower.estimated_cost_gt)
        && upper_subsumes(broader.estimated_cost_lt, narrower.estimated_cost_lt)
        && lower_subsumes(
            broader.upstream_latency_ms_p95_gt.map(f64::from),
            narrower.upstream_latency_ms_p95_gt.map(f64::from),
        )
        && (!broader.not_reasoning_class || narrower.not_reasoning_class)
}

fn exact_option_subsumes<T: PartialEq>(broader: &Option<T>, narrower: &Option<T>) -> bool {
    broader.is_none()
        || (narrower.is_some() && broader.as_ref().unwrap() == narrower.as_ref().unwrap())
}

fn exact_option_overlap<T: PartialEq>(left: &Option<T>, right: &Option<T>) -> bool {
    left.is_none() || right.is_none() || left.as_ref().unwrap() == right.as_ref().unwrap()
}

/// `conditionRegionsMayOverlap`: whether the two independent safe fragments
/// could co-match at least one request (conservative — does not prove a
/// concrete request exists).
fn regions_may_overlap(left: &DashboardSafeRegion, right: &DashboardSafeRegion) -> bool {
    let sets_overlap = |l: &Option<BTreeSet<String>>, r: &Option<BTreeSet<String>>| match (l, r) {
        (None, _) | (_, None) => true,
        (Some(l), Some(r)) => l.iter().any(|value| r.contains(value)),
    };
    let strict_u32 = |lg: Option<u32>, ll: Option<u32>, rg: Option<u32>, rl: Option<u32>| {
        let lower = i64::max(lg.map_or(-1, i64::from), rg.map_or(-1, i64::from));
        let upper_exclusive = i64::min(
            ll.map_or((U32_MAX + 1) as i64, |v| v as i64),
            rl.map_or((U32_MAX + 1) as i64, |v| v as i64),
        );
        lower + 1 < upper_exclusive
    };
    let strict_number = |lg: Option<f64>, ll: Option<f64>, rg: Option<f64>, rl: Option<f64>| {
        let lower = lg
            .unwrap_or(f64::NEG_INFINITY)
            .max(rg.unwrap_or(f64::NEG_INFINITY));
        let upper = ll.unwrap_or(f64::INFINITY).min(rl.unwrap_or(f64::INFINITY));
        lower < upper
    };

    sets_overlap(&left.model_in, &right.model_in)
        && exact_option_overlap(&left.tag_equals, &right.tag_equals)
        && exact_option_overlap(&left.has_images, &right.has_images)
        && exact_option_overlap(&left.has_audio, &right.has_audio)
        && exact_option_overlap(&left.has_documents, &right.has_documents)
        && exact_option_overlap(&left.content_type, &right.content_type)
        && strict_u32(
            left.input_tokens_gt,
            left.input_tokens_lt,
            right.input_tokens_gt,
            right.input_tokens_lt,
        )
        && strict_number(
            left.estimated_cost_gt,
            left.estimated_cost_lt,
            right.estimated_cost_gt,
            right.estimated_cost_lt,
        )
        && (left.prompt_keywords.is_none()
            || right.prompt_keywords.is_none()
            || (!left.prompt_keywords.as_ref().unwrap().is_empty()
                && !right.prompt_keywords.as_ref().unwrap().is_empty()))
        && i64::max(
            left.upstream_latency_ms_p95_gt.map_or(-1, i64::from),
            right.upstream_latency_ms_p95_gt.map_or(-1, i64::from),
        ) < U32_MAX as i64
}

/// One dashboard priority-decision step mirroring
/// `priorityConditionFindingsFromCandidates` for a single
/// (same-or-higher-priority, non-identical) candidate pair. `None` region on
/// either side is `unresolved` — the dashboard claims nothing.
enum PairClassification {
    /// Strictly higher priority and the higher-priority candidate subsumes the
    /// subject: proven shadow (`higher_priority_conditions_subsume`).
    Subsumes,
    /// Same-or-higher priority, not provably disjoint: conservative overlap
    /// (`higher_priority_conditions_may_overlap` /
    /// `equal_priority_conditions_may_overlap`).
    MayOverlap,
    /// Independently proven disjoint — no finding is emitted.
    Disjoint,
}

fn classify_pair(
    candidate: &DashboardSafeRegion,
    candidate_priority: u32,
    subject: &DashboardSafeRegion,
    subject_priority: u32,
) -> PairClassification {
    if candidate_priority > subject_priority && region_subsumes(candidate, subject) {
        return PairClassification::Subsumes;
    }
    if regions_may_overlap(candidate, subject) {
        PairClassification::MayOverlap
    } else {
        PairClassification::Disjoint
    }
}

// ---------------------------------------------------------------------------
// Shared feature-snapshot builder.
// ---------------------------------------------------------------------------

/// Build the canonical matcher snapshot from a corpus `features` value. Only
/// features named in the corpus are observed; the rest stay unavailable
/// (fail-closed), exactly like the retained-feature decision path.
fn snapshot_from_features(features: &Value) -> RouteFeatureSnapshot {
    let obj = features.as_object().expect("features must be an object");
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unused-model")
        .to_owned();
    let input_tokens = obj.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32;
    let tag = obj.get("tag").and_then(Value::as_str).map(str::to_owned);

    let mut snapshot = RouteFeatureSnapshot::from_retained_features(model, input_tokens, tag);

    if let Some(modalities) = ["has_images", "has_audio", "has_documents"]
        .iter()
        .find(|key| obj.contains_key(**key))
    {
        let flag = |key: &str| obj.get(key).and_then(Value::as_bool).unwrap_or(false);
        match *modalities {
            "has_images" => {
                snapshot = snapshot.with_modalities(
                    obj.get("has_images")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    flag("has_audio"),
                    flag("has_documents"),
                );
            }
            "has_audio" => {
                snapshot = snapshot.with_modalities(
                    flag("has_images"),
                    obj.get("has_audio")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    flag("has_documents"),
                );
            }
            _ => {
                snapshot = snapshot.with_modalities(
                    flag("has_images"),
                    flag("has_audio"),
                    obj.get("has_documents")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                );
            }
        }
    }

    if let Some(content_type) = obj.get("content_type") {
        snapshot = snapshot.with_content_type(content_type.as_str().map(str::to_owned));
    }
    if let Some(prompt) = obj.get("prompt").and_then(Value::as_str) {
        snapshot = snapshot.with_input_text(prompt);
    }
    if let Some(cost) = obj.get("estimated_cost_usd").and_then(Value::as_f64) {
        snapshot = snapshot.with_exact_estimated_cost_usd(cost);
    }
    if let Some(p95) = obj.get("upstream_p95_ms").and_then(Value::as_u64) {
        snapshot = snapshot.with_observed_p95_ms(p95 as u32);
    }
    if let Some(reasoning) = obj.get("is_reasoning_class").and_then(Value::as_bool) {
        snapshot = snapshot.with_reasoning_class(reasoning);
    }
    snapshot
}

fn conditions_from_value(value: &Value) -> RouteConditions {
    serde_json::from_value(value.clone())
        .unwrap_or_else(|e| panic!("conditions must decode into RouteConditions: {e}"))
}

fn assert_output(case_id: &str, expected: &str, outcome: &RouteConditionOutcome) {
    let actual = match outcome {
        RouteConditionOutcome::Inactive => "inactive",
        RouteConditionOutcome::Matched => "matched",
        RouteConditionOutcome::NotMatched => "not_matched",
        RouteConditionOutcome::Unavailable => "unavailable",
    };
    assert_eq!(
        actual, expected,
        "case {case_id}: canonical matcher outcome mismatch (use --nocapture for details)"
    );
}

/// Assert the corpus `expected` region (object or null) matches the ported
/// dashboard translation for one conditions value.
fn assert_dashboard_region(case_id: &str, conditions: &Value, expected: &Option<Value>) {
    let actual = safe_condition_region(conditions);
    let Some(expected) = expected else {
        assert!(
            actual.is_none(),
            "case {case_id}: dashboard safe fragment must fail closed (region null), but got a region"
        );
        return;
    };
    let actual = actual
        .unwrap_or_else(|| panic!("case {case_id}: expected a dashboard region, got fail-closed"));
    let expected = expected
        .as_object()
        .expect("expected region must be an object");

    let expected_text = |key: &str| expected.get(key).and_then(Value::as_str);
    let expected_u32 = |key: &str| expected.get(key).and_then(Value::as_u64).map(|v| v as u32);
    let expected_f64 = |key: &str| expected.get(key).and_then(Value::as_f64);
    let expected_bool = |key: &str| expected.get(key).and_then(Value::as_bool);

    let expected_set: Option<BTreeSet<String>> = expected.get("model_in").map(|members| {
        members
            .as_array()
            .expect("model_in must be an array")
            .iter()
            .map(|m| m.as_str().expect("string member").to_owned())
            .collect()
    });
    assert_eq!(
        actual.model_in, expected_set,
        "case {case_id}: model_in region mismatch"
    );
    assert_eq!(
        actual.input_tokens_gt,
        expected_u32("input_tokens_gt"),
        "case {case_id}: input_tokens_gt region mismatch"
    );
    assert_eq!(
        actual.input_tokens_lt,
        expected_u32("input_tokens_lt"),
        "case {case_id}: input_tokens_lt region mismatch"
    );
    assert_eq!(
        actual.tag_equals.as_deref(),
        expected_text("tag_equals"),
        "case {case_id}: tag_equals region mismatch"
    );
    assert_eq!(
        actual.has_images,
        expected_bool("has_images"),
        "case {case_id}: has_images region mismatch"
    );
    assert_eq!(
        actual.has_audio,
        expected_bool("has_audio"),
        "case {case_id}: has_audio region mismatch"
    );
    assert_eq!(
        actual.has_documents,
        expected_bool("has_documents"),
        "case {case_id}: has_documents region mismatch"
    );
    assert_eq!(
        actual.content_type.as_deref(),
        expected_text("content_type"),
        "case {case_id}: content_type region mismatch"
    );
    if let Some(keywords) = expected.get("prompt_keywords") {
        let expected_keywords: Vec<String> = keywords
            .as_array()
            .expect("prompt_keywords must be an array")
            .iter()
            .map(|k| k.as_str().expect("string keyword").to_owned())
            .collect();
        assert_eq!(
            actual.prompt_keywords.as_deref(),
            Some(expected_keywords.as_slice()),
            "case {case_id}: prompt_keywords region mismatch"
        );
    } else {
        assert!(
            actual.prompt_keywords.is_none(),
            "case {case_id}: unexpected prompt_keywords in region"
        );
    }
    assert_eq!(
        actual.estimated_cost_gt,
        expected_f64("estimated_cost_gt"),
        "case {case_id}: estimated_cost_gt region mismatch"
    );
    assert_eq!(
        actual.estimated_cost_lt,
        expected_f64("estimated_cost_lt"),
        "case {case_id}: estimated_cost_lt region mismatch"
    );
    assert_eq!(
        actual.upstream_latency_ms_p95_gt,
        expected_u32("upstream_latency_ms_p95_gt"),
        "case {case_id}: upstream_latency_ms_p95_gt region mismatch"
    );
    assert_eq!(
        actual.not_reasoning_class,
        expected_bool("not_reasoning_class").unwrap_or(false),
        "case {case_id}: not_reasoning_class region mismatch"
    );
}

// ---------------------------------------------------------------------------
// Tests: closed condition-field pin.
// ---------------------------------------------------------------------------

#[test]
fn corpus_pins_the_closed_condition_field_set_in_canonical_order() {
    let corpus = corpus();
    assert_eq!(corpus.corpus.id, CORPUS_FORMAT_ID);
    assert_eq!(corpus.corpus.version, CORPUS_FORMAT_VERSION);
    assert_eq!(corpus.route_contract.id, ROUTE_SCHEMA_ID);
    assert_eq!(corpus.route_contract.version, ROUTE_SCHEMA_VERSION);

    let canonical: Vec<&str> = RouteConditionField::ALL
        .iter()
        .copied()
        .map(RouteConditionField::as_str)
        .collect();
    let pinned: Vec<&str> = corpus
        .condition_fields
        .iter()
        .map(|field| field.field.as_str())
        .collect();
    assert_eq!(
        pinned, canonical,
        "the pinned condition-field set must equal RouteConditionField::ALL in wire declaration order"
    );
    assert_eq!(canonical.len(), 13);
    assert_eq!(
        CLOSED_CONDITION_KEYS.as_slice(),
        canonical,
        "the dashboard-safe closed key set must match the canonical field names"
    );
}

// ---------------------------------------------------------------------------
// Tests: non-ASCII prompt parity.
// ---------------------------------------------------------------------------

#[test]
fn prompt_parity_non_ascii_lowercase_matches_runtime_and_fails_closed_in_dashboard() {
    let corpus = corpus();
    for prompt_case in &corpus.prompt_cases {
        let conditions = serde_json::json!({
            "prompt_contains_any_of": prompt_case.keywords,
        });
        let conditions = conditions_from_value(&conditions);
        let snapshot =
            RouteFeatureSnapshot::from_retained_features("unused-model".to_owned(), 10, None)
                .with_input_text(&prompt_case.prompt);

        let evaluation = tt_routing::evaluate_route_conditions(&conditions, &snapshot);
        let prompt_decision = evaluation
            .decisions
            .iter()
            .find(|d| d.field == RouteConditionField::PromptContainsAnyOf)
            .expect("prompt decision present");
        match prompt_decision.outcome {
            RouteConditionOutcome::Matched => assert!(
                prompt_case.expected_match,
                "{}: matcher matched but corpus expects no match",
                prompt_case.id
            ),
            RouteConditionOutcome::NotMatched => assert!(
                !prompt_case.expected_match,
                "{}: matcher did not match but corpus expects a match",
                prompt_case.id
            ),
            other => panic!(
                "{}: unexpected prompt outcome {other:?} — the keyword must be active",
                prompt_case.id
            ),
        }

        // The dashboard translation over the same decoded conditions must agree
        // with the corpus (admitted ASCII keyword regions, non-ASCII fail-closed).
        let raw_conditions = serde_json::json!({ "prompt_contains_any_of": prompt_case.keywords });
        assert_dashboard_region(
            &prompt_case.id,
            &raw_conditions,
            &prompt_case.dashboard_region,
        );
    }
}

// ---------------------------------------------------------------------------
// Tests: closed condition-field decode/trim/replace parity.
// ---------------------------------------------------------------------------

#[test]
fn field_cases_match_canonical_matcher_and_dashboard_translation() {
    let corpus = corpus();
    for field_case in &corpus.field_cases {
        let conditions = conditions_from_value(&field_case.conditions);
        let snapshot = snapshot_from_features(&field_case.features);
        let evaluation = tt_routing::evaluate_route_conditions(&conditions, &snapshot);

        let decision = evaluation
            .decisions
            .iter()
            .find(|d| d.field.as_str() == field_case.field)
            .unwrap_or_else(|| {
                panic!(
                    "{}: no canonical decision for field {}",
                    field_case.id, field_case.field
                )
            });
        assert_output(
            &field_case.id,
            &field_case.expected_outcome,
            &decision.outcome,
        );
        assert_eq!(
            evaluation.matches(),
            field_case.expected_match,
            "{}: overall conjunction mismatch",
            field_case.id
        );
        assert_dashboard_region(
            &field_case.id,
            &field_case.conditions,
            &field_case.dashboard_region,
        );
    }
}

#[test]
fn fail_closed_cases_never_produce_a_safe_region() {
    let corpus = corpus();
    for fail_closed in &corpus.fail_closed_cases {
        assert!(
            safe_condition_region(&fail_closed.conditions).is_none(),
            "{}: the dashboard safe fragment must fail closed (region null) for {:?}",
            fail_closed.id,
            fail_closed.conditions
        );
    }
}

#[test]
fn cap_interactions_are_identical_between_matcher_and_translation() {
    // The dashboard-safe parser caps arrays at MAX_CANONICAL_ITEMS and each
    // prompt keyword at MAX_CANONICAL_CHARS ASCII chars. The corpus keeps the
    // JSON readable; these bounds are built here so a constant drift fails the
    // parity test rather than silently changing the closed fragment.
    let mut models_ok = serde_json::Map::new();
    models_ok.insert(
        "model_in".to_owned(),
        Value::Array(
            (0..MAX_CANONICAL_ITEMS)
                .map(|_| Value::String("m".to_owned()))
                .collect(),
        ),
    );
    assert!(
        safe_condition_region(&Value::Object(models_ok)).is_some(),
        "exactly MAX_CANONICAL_ITEMS models is still a safe region"
    );

    let mut models_over = serde_json::Map::new();
    models_over.insert(
        "model_in".to_owned(),
        Value::Array(
            (0..MAX_CANONICAL_ITEMS + 1)
                .map(|_| Value::String("m".to_owned()))
                .collect(),
        ),
    );
    assert!(
        safe_condition_region(&Value::Object(models_over)).is_none(),
        "MAX_CANONICAL_ITEMS + 1 models must fail closed"
    );

    let keyword_at_cap = "a".repeat(MAX_CANONICAL_CHARS);
    let keyword_over_cap = "a".repeat(MAX_CANONICAL_CHARS + 1);
    let region_at = safe_condition_region(&serde_json::json!({
        "prompt_contains_any_of": [keyword_at_cap],
    }));
    assert!(
        region_at.is_some(),
        "a keyword at MAX_CANONICAL_CHARS ASCII chars is a safe region"
    );
    assert!(
        safe_condition_region(&serde_json::json!({
            "prompt_contains_any_of": [keyword_over_cap],
        }))
        .is_none(),
        "a keyword over MAX_CANONICAL_CHARS must fail closed"
    );

    // Mixed ASCII + non-ASCII keyword set fails closed as a whole (the
    // dashboard never partially reasons about a set it cannot fully admit).
    assert!(
        safe_condition_region(&serde_json::json!({
            "prompt_contains_any_of": ["ok", "café"],
        }))
        .is_none(),
        "a partially non-ASCII keyword set must fail closed"
    );
}

// ---------------------------------------------------------------------------
// Tests: overlapping equal-priority consistency + store-order winners.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct OverlapRoute {
    spec: OverlapRouteSpec,
    conditions: RouteConditions,
}

fn route_for(route: &OverlapRoute, target: &str) -> Route {
    Route {
        id: Uuid::parse_str(&route.spec.id).unwrap_or_else(|e| {
            panic!(
                "{}: invalid corpus UUID {}: {e}",
                route.spec.name, route.spec.id
            )
        }),
        name: route.spec.name.clone(),
        priority: route.spec.priority,
        enabled: true,
        when: route.conditions.clone(),
        then: RouteAction {
            target_model: Some(target.to_owned()),
            ..Default::default()
        },
        paused: false,
    }
}

/// Persisted `priority DESC, created_at ASC, id ASC` store order. Equal
/// priorities keep creation order; identical creation keeps UUID order.
fn store_order(routes: &[OverlapRoute]) -> Vec<OverlapRoute> {
    let mut ordered: Vec<OverlapRoute> = routes.to_vec();
    ordered.sort_by(|a, b| {
        b.spec
            .priority
            .cmp(&a.spec.priority)
            .then_with(|| a.spec.created_at.cmp(&b.spec.created_at))
            .then_with(|| a.spec.id.to_lowercase().cmp(&b.spec.id.to_lowercase()))
    });
    ordered
}

#[test]
fn overlap_cases_flag_equal_priority_overlap_and_pick_the_store_order_winner() {
    let corpus = corpus();
    for overlap in &corpus.overlap_cases {
        let routes: Vec<OverlapRoute> = overlap
            .routes
            .iter()
            .map(|spec| OverlapRoute {
                spec: spec.clone(),
                conditions: conditions_from_value(&spec.conditions),
            })
            .collect();

        let by_name: std::collections::HashMap<&str, &OverlapRoute> =
            routes.iter().map(|r| (r.spec.name.as_str(), r)).collect();

        // Dashboard translation: one region per route.
        let regions: Vec<Option<DashboardSafeRegion>> = overlap
            .routes
            .iter()
            .map(|spec| safe_condition_region(&spec.conditions))
            .collect();

        // Every corpus pair must classify exactly as the dashboard translation
        // says (subsumes / may_overlap / disjoint / unresolved).
        for pair in &overlap.pairs {
            let candidate_route = by_name[pair.candidate.as_str()];
            let subject_route = by_name[pair.subject.as_str()];
            let candidate_region = regions
                .iter()
                .zip(overlap.routes.iter())
                .find(|(_, spec)| spec.name == pair.candidate)
                .map(|(r, _)| r)
                .expect("candidate route present")
                .as_ref();
            let subject_region = regions
                .iter()
                .zip(overlap.routes.iter())
                .find(|(_, spec)| spec.name == pair.subject)
                .map(|(r, _)| r)
                .expect("subject route present")
                .as_ref();

            let classification = match (candidate_region, subject_region) {
                (None, _) | (_, None) => "unresolved",
                (Some(candidate_region), Some(subject_region)) => match classify_pair(
                    candidate_region,
                    candidate_route.spec.priority,
                    subject_region,
                    subject_route.spec.priority,
                ) {
                    PairClassification::Subsumes => "subsumes",
                    PairClassification::MayOverlap => "may_overlap",
                    PairClassification::Disjoint => "disjoint",
                },
            };
            assert_eq!(
                classification, pair.expected,
                "{}: pair {}/{} dashboard classification mismatch",
                overlap.id, pair.candidate, pair.subject
            );
            assert_eq!(
                pair.relation,
                if candidate_route.spec.priority > subject_route.spec.priority {
                    "higher"
                } else {
                    "equal"
                },
                "{}: pair {}/{} declared priority relation does not match the routes",
                overlap.id,
                pair.candidate,
                pair.subject
            );
        }

        // Real engine: store-order first-match-wins must select the corpus
        // winner for every request.
        let engine = RoutingEngine::with_routes(
            store_order(&routes)
                .iter()
                .map(|route| route_for(route, &route.spec.name)),
        );

        for request in &overlap.requests {
            let snapshot = snapshot_from_features(&request.features);
            let selected = engine.evaluate_snapshot(&snapshot).unwrap_or_else(|| {
                panic!(
                    "{}: no route selected for request {}",
                    overlap.id, request.id
                )
            });
            assert_eq!(
                selected.name, request.expected_winner,
                "{}: engine winner for request {} does not match the corpus",
                overlap.id, request.id
            );
        }

        // Consistency invariant: whenever the engine's request co-matches
        // multiple routes, the dashboard translation must NOT claim those
        // routes are provably disjoint — runtime overlap implies "not proven
        // disjoint" (this is the equal-priority flag consistency guarantee).
        for request in &overlap.requests {
            let snapshot = snapshot_from_features(&request.features);
            let matched: Vec<&str> = routes
                .iter()
                .filter(|route| tt_routing::route_conditions_match(&route.conditions, &snapshot))
                .map(|route| route.spec.name.as_str())
                .collect();
            if matched.len() <= 1 {
                continue;
            }
            for i in 0..matched.len() {
                for j in (i + 1)..matched.len() {
                    let left = by_name[matched[i]];
                    let right = by_name[matched[j]];
                    let left_region = safe_condition_region(&left.spec.conditions);
                    let right_region = safe_condition_region(&right.spec.conditions);
                    let classification = match (&left_region, &right_region) {
                        (Some(l), Some(r)) => {
                            match classify_pair(l, left.spec.priority, r, right.spec.priority) {
                                PairClassification::Disjoint => "disjoint",
                                _ => "not_disjoint",
                            }
                        }
                        _ => "not_disjoint",
                    };
                    assert_ne!(
                        classification, "disjoint",
                        "{}: request {} co-matches {} and {} but the dashboard translation proves them disjoint — the conservative flag contract is violated",
                        overlap.id, request.id, matched[i], matched[j]
                    );
                }
            }
        }
    }
}
