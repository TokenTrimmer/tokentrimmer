//! Per-request telemetry writer.
//!
//! Backs the `request_logs` table from `crates/core/migrations/0001_request_logs.up.sql`.
//! The gateway spawns a fire-and-forget call to [`RequestLogWriter::write`]
//! after every response so the dashboard's `/api/telemetry` endpoints
//! and the Plan replay engine have a uniform input.
//!
//! Why a trait rather than just sqlx everywhere: tests need to assert
//! "what did the gateway log?" without spinning up Postgres. The
//! [`InMemoryRequestLogWriter`] in this module is what those tests use;
//! production wires [`PostgresRequestLogWriter`] behind the `postgres`
//! feature.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Row to insert into `request_logs`. Field names mirror the SQL column
/// names so the writer impls don't need a translation layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLogRow {
    pub id: Uuid,
    pub org_id: Uuid,
    pub api_key_id: Uuid,
    pub ts: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    /// Cached input tokens. `0` when nothing was served from cache.
    pub cached_tokens: i32,
    pub cost_usd: f64,
    pub baseline_cost_usd: f64,
    /// Provider-side automatic prompt-cache discount (USD) — savings the
    /// provider grants with or without TokenTrimmer. Kept separate so the
    /// TT-attributed saving (`baseline_cost_usd - cost_usd -
    /// provider_cache_saved_usd - cache_bust_penalty_usd`) survives invoice
    /// reconciliation. `0.0` for TT cache hits (no provider call) and rows
    /// from before migration 0011.
    #[serde(default)]
    pub provider_cache_saved_usd: f64,
    /// Estimated USD penalty of a deliberate, non-deterministic stable-prefix
    /// mutation (a booked `CacheBustEstimate`) — the NEGATIVE savings entry.
    /// Persisted so the row-derived TT headline (see `provider_cache_saved_usd`
    /// formula above) matches the `x-tokentrimmer-saved-usd` header / span on
    /// every request. An estimate of induced future cost: NEVER folded into
    /// `cost_usd` / `baseline_cost_usd`. `0.0` when no bust booked (all
    /// current traffic — redaction is ingress-deterministic and busts
    /// nothing), for TT cache hits, and for rows before migration 0016.
    #[serde(default)]
    pub cache_bust_penalty_usd: f64,
    /// `true` when ANY cache layer served the response (L1 or L2).
    pub cached: bool,
    /// `Some("l1")` / `Some("l2")` / `None`. Matches the SQL CHECK constraint.
    pub cache_layer: Option<String>,
    pub route_id: Option<Uuid>,
    pub latency_ms: i32,
    pub upstream_latency_ms: Option<i32>,
    pub status: i32,
    pub tag: Option<String>,
    pub error_class: Option<String>,
    pub trace_id: Option<String>,
    /// `true` when the SSE stream was dropped before a `finish_reason` chunk
    /// arrived (client abort or upstream reset). `false` for non-streaming
    /// responses and for streams that completed cleanly.
    #[serde(default)]
    pub truncated: bool,
    /// Canary shadow-mode candidate model (`RouteAction::shadow_model`) that the
    /// gateway ALSO dispatched for this request, discarding its response. `None`
    /// for the overwhelming majority of rows (shadow is opt-in per route) and for
    /// rows written before migration 0012. Kept SEPARATE from `model` (the served
    /// model) so the shadow arm is auditable without inflating served-traffic
    /// stats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_model: Option<String>,
    /// Cost (USD) the discarded shadow dispatch incurred. Recorded in its OWN
    /// column — never folded into `cost_usd` — so reconciliation can attribute
    /// the extra (doubled) spend to the canary experiment rather than to served
    /// traffic. `None` when no shadow fired (and for rows before migration 0012).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_cost_usd: Option<f64>,
    /// Which arm the `RouteAction::traffic_pct` canary split assigned this
    /// request to: `Some("canary")` (routed to the new `target_model`),
    /// `Some("control")` (passed through on the original model), or `None` when
    /// the route declared no `traffic_pct` (unconditional rewrite) / no route
    /// matched (and for rows before migration 0012). Lets dashboards compare the
    /// two arms' cost/quality without re-deriving the sticky hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic_split_arm: Option<String>,
    /// Raw provider-reported cache-read input tokens. `None` (-> SQL NULL) when
    /// the provider did not report the field OR no provider call was made (TT
    /// L1/L2 hits, truncated streams with no terminal usage). `Some(0)` means
    /// the provider explicitly reported zero. Rows from before migration 0015
    /// are NULL. The NOT NULL `cached_tokens` above keeps its folded
    /// (absent => 0) semantics for back-compat.
    ///
    /// One deliberate exception (streamed fold-rescue): a terminal usage chunk
    /// with folded `cached_tokens > 0` but the raw field absent (pre-fix
    /// adapter / older TT hop) is recorded as `Some(fold)` — a nonzero fold
    /// proves real provider cache reads and must not regress to NULL. A folded
    /// 0 without the raw field stays `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<i32>,
    /// Raw provider-reported cache-write (creation) input tokens. Same NULL
    /// semantics; Anthropic-only in practice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<i32>,
    /// `true` when a matched route's advisory **batch-eligibility** marker
    /// (`RouteAction::batch`, research Phase 2.1) survived the gateway's
    /// hard-ineligibility gate (non-streaming, non-interactive, served model
    /// carries a catalog batch tier check at mark time). The gateway is
    /// synchronous today, so an eligible row was STILL dispatched and billed
    /// normally — this marks route intent for the future async Batch Lane.
    /// `false` for unmarked / streaming / interactive traffic, TT cache hits,
    /// and rows from before migration 0017.
    #[serde(default)]
    pub batch_eligible: bool,
    /// FORGONE Batch-API discount (USD) for a batch-eligible request: realized
    /// `cost_usd` minus the served model's catalog batch-rate cost on the full
    /// prompt+completion, floored at 0, fee-applied. A projection of what the
    /// async Batch Lane would have saved — NEVER part of `cost_usd` /
    /// `baseline_cost_usd` / the TT saved-usd headline (those reconcile against
    /// the provider invoice). `/costs` + the digest may
    /// `SUM(batch_forgone_usd) WHERE batch_eligible` to say "$X/mo would be
    /// saved by the Batch Lane". `0.0` when not batch-eligible, when the served
    /// model carries no catalog batch tier (e.g. post-failover — no real rate,
    /// no claim), and for rows from before migration 0017.
    #[serde(default)]
    pub batch_forgone_usd: f64,
    /// `true` when a matched route's rewrite was suppressed by a sticky pause
    /// (research Phase 2.3 auto-pause / `POST /v1/routes/:id/pause`): the
    /// request flowed to the ORIGINALLY-requested model with every cost lever
    /// off. The route still attributes (`route_id` is stamped) so paused
    /// traffic is auditable per route; cost == baseline on these rows (no
    /// fabricated saving). `false` for every unrouted/unpaused request and
    /// rows from before migration 0019 (mirror of `truncated`).
    #[serde(default)]
    pub route_paused: bool,
    /// ESTIMATED saving from minified-JSON output steering
    /// (`RouteAction::minify_json`, research Phase 3.1, migration 0020): the
    /// pretty-printed re-rendering of the emitted JSON re-tokenized with the
    /// served model's tokenizer, minus the tokens actually emitted, priced at
    /// the billed output rate (fee-applied). An ESTIMATE of an unmeasurable
    /// counterfactual: NEVER part of `cost_usd` / `baseline_cost_usd` / the
    /// saved-usd headline (those reconcile against the provider invoice).
    /// `0.0` when the instruction was not injected, when the response was not
    /// valid JSON, for streaming responses (v1 meters but does not estimate),
    /// TT cache hits, and rows predating migration 0020 (zero omitted on
    /// serialize so legacy row JSON stays byte-identical).
    #[serde(default, skip_serializing_if = "f64_is_zero")]
    pub minify_saved_est_usd: f64,
    /// `Some("csv")` / `Some("bare")` when a route's `format_switch` action
    /// VALIDATED on this response — the caller received the switched (CSV /
    /// bare-value) body instead of verbose JSON, advertised via the
    /// `format_switch:<label>` warnings token. `None` for unswitched traffic,
    /// fail-open passthroughs (`format_switch_failed:*`), and rows from
    /// before migration 0020.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_switched: Option<String>,
    /// LABELED ESTIMATE (USD) of the format-switch saving: tokens of a
    /// JSON-equivalent reconstruction minus tokens of the emitted body, priced
    /// at the served model's output rate, fee-applied. NEVER part of
    /// `cost_usd` / `baseline_cost_usd` / the saved-usd headline (those
    /// reconcile against the provider invoice — a reconstruction is not an
    /// invoice figure). `0.0` when no switch validated, when the
    /// reconstruction was not computable (booked $0 + metered), and for rows
    /// from before migration 0020.
    #[serde(default)]
    pub format_switch_saved_est_usd: f64,
    /// `true` when a route's `diff` action applied: the model emitted an
    /// anchored search/replace patch, the gateway validated + applied it to
    /// the prior and returned the FULL reconstructed artifact (caller contract
    /// preserved; `diff_applied` warnings token). The row's token counts /
    /// `cost_usd` are the PROVIDER-BILLED patch figures — the savings being
    /// real is the point. `false` for rows from before migration 0020.
    #[serde(default)]
    pub diff_applied: bool,
    /// MEASURED diff saving (USD): the output tokens the patch avoided billing
    /// (tokenized reconstructed artifact − billed patch completion tokens) at
    /// the served model's output rate, fee-applied. Both sides are real
    /// tokenizer counts on real strings, so this is included in the saved-usd
    /// headline via the baseline fold (compression precedent). `0.0` when no
    /// diff applied and for rows from before migration 0020.
    #[serde(default)]
    pub diff_saved_usd: f64,
    /// `true` when a `diff` patch FAILED validation and the gateway failed
    /// CLOSED to a full re-emit (or passed the raw patch through marked
    /// `diff_degraded` when the re-emit itself errored). `false` for rows
    /// from before migration 0020.
    #[serde(default)]
    pub diff_failed: bool,
    /// Realized cost (USD, fee-applied) of the FAILED patch attempt on a
    /// fail-closed double dispatch. FOLDED into `cost_usd` (it is real invoice
    /// spend for this trace) AND kept here so a CFO can unpick the retry tax.
    /// `0.0` when no diff failed and for rows from before migration 0020.
    #[serde(default)]
    pub diff_failed_cost_usd: f64,
    /// NET token delta from retrieval substitution (`<retrievable>` tags):
    /// original tag payload tokens minus retrieved replacement tokens, minus
    /// query-embedding token cost. This is token-denominated and may be
    /// negative; it is deliberately NOT converted into USD or folded into the
    /// invoice-reconciled saved-usd headline. `0` means retrieval did not run
    /// for the request, and is the default for rows predating migration 0023.
    #[serde(default)]
    pub retrieval_tokens_saved: i64,
    /// Document Lane D2: pipeline-MEASURED input tokens the lossless
    /// document-compaction pass (`RouteAction::doc_compaction`) removed from
    /// LARGE non-prose documents before dispatch (token-true-gated, text-only).
    /// Token-denominated; the USD value folds into the saved-usd headline via
    /// the same baseline fold as compression and is surfaced on its own
    /// `X-TokenTrimmer-Doc-Compaction-Saved-Usd` header. `0` when the route did
    /// not opt into doc_compaction and for rows predating migration 0031.
    #[serde(default)]
    pub doc_compaction_tokens_removed: i64,
    /// TR-2 (migration 0037): the MEASURED USD value (fee-applied) of the
    /// input tokens the lossless conservative `compress` pass
    /// (`RouteAction::compress`) removed before dispatch —
    /// `pass_effects.compression_tokens_removed × input rate × fee`. Folds into
    /// the saved-usd headline via the same baseline fold as
    /// `doc_compaction_tokens_removed` (the removed tokens raise
    /// `baseline_cost_usd`, so the saving rides `baseline − cost`). Surfaced on
    /// `X-TokenTrimmer-Compression-Saved-Usd`. `0.0` when the route did not opt
    /// into `compress` and for rows predating migration 0037.
    #[serde(default)]
    pub compression_saved_usd: f64,
    /// TR-2 (migration 0037): the pipeline-MEASURED input-token count the
    /// conservative `compress` pass removed (token-true-gated). The
    /// token-denominated companion to `compression_saved_usd`, for methodology
    /// reconciliation + the per-request compression waterfall (TR-1). `0` when
    /// the route did not opt into `compress` and for rows predating
    /// migration 0037.
    #[serde(default)]
    pub compression_tokens_removed: i64,
    /// Document Lane D4: ISOLATED, ESTIMATED vision-avoided saving (USD, migration
    /// 0032) — the counterfactual value of swapping an image/document part for
    /// distilled text at the pre-routing seam (raw image tokens that WOULD have
    /// been billed minus the distilled text tokens, at the input rate; $0 for
    /// Gemini per the D0 direction guard). NEVER part of `cost_usd` /
    /// `baseline_cost_usd` / the saved-usd headline (the dispatched request never
    /// contained the image, so it is not invoice-reconcilable). Surfaced on
    /// `X-TokenTrimmer-Doc-Vision-Saved-Est-Usd`. `0.0` in D4a (the seam that
    /// books a non-zero value is D4c) and for rows predating migration 0032 (zero
    /// omitted on serialize so legacy row JSON stays byte-identical).
    #[serde(default, skip_serializing_if = "f64_is_zero")]
    pub doc_vision_saved_est_usd: f64,
    /// UUID of the durable agent run this request belongs to (W0b migration
    /// 0027). `None` for single-turn (non-agentic) requests and for rows
    /// written before the agent-run grain shipped. Stamped by the agentic
    /// loop in tt-core (Task 4); `None` at all non-agent call sites.
    #[serde(default)]
    pub run_id: Option<Uuid>,
    /// UUID of the specific plan node / tool-call step within the agent run.
    /// `None` for single-turn requests and for rows written before migration
    /// 0027. Stamped alongside `run_id` by the agentic loop (Task 4).
    #[serde(default)]
    pub node_id: Option<Uuid>,
    /// Content-aware compression (P1a, migration 0033): ISOLATED, ESTIMATED USD
    /// value of the input tokens the content_compress structural backend removed
    /// (tokens removed × the served model's input rate, fee-applied). Like
    /// `doc_vision_saved_est_usd` it is NEVER part of `cost_usd` /
    /// `baseline_cost_usd` / the saved-usd headline — a conservative estimate,
    /// not an invoice-reconciled figure. Surfaced on
    /// `X-TokenTrimmer-Content-Compress-Saved-Est-Usd`. `0.0` when the route did
    /// not opt into `content_compress` and for rows predating migration 0033
    /// (zero omitted on serialize so legacy row JSON stays byte-identical).
    #[serde(default, skip_serializing_if = "f64_is_zero")]
    pub content_compress_saved_est_usd: f64,
    /// Content-aware compression flywheel (P1a, migration 0033): the DOMINANT
    /// content kind the content_compress backend compacted on this request
    /// (`"json"` / `"csv"` / `"log"`), or `None` when the route did not opt in,
    /// nothing was compacted, or for rows predating migration 0033. Metrics-only
    /// label (no request content) — the ZDR-safe flywheel signal; the opt-in raw
    /// before/after pair capture is a separate, off-by-default path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_compress_kind: Option<String>,

    // ── L2 (semantic-cache) hit provenance (migration 0035) ───────────────
    // The three fields below persist the cache-hit provenance a signed L2 receipt
    // (tt_telemetry::l2_receipt) attests. Set ONLY by `request_log_for_l2_hit`
    // (the L2-hit row); None for every other row (L1 hits, dispatches, tests).
    // Stored as Options so legacy rows + non-L2 requests stay byte-identical
    // (skipped on serialize when None). The cloud mint endpoint
    // (POST /v1/admin/requests/{trace_id}/l2-receipt/sign) reads these off the
    // row + signs them with the audit key — mirroring the VCR mint endpoint.
    /// The cache entry the query matched (`cache_entries.id`). None when this
    /// row was not an L2 hit. Lets the customer attribute the hit to a specific
    /// prior answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l2_matched_entry_id: Option<Uuid>,
    /// The cosine similarity of the match (0.0–1.0). None for non-L2 rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l2_similarity: Option<f32>,
    /// The L2 verify-gate verdict code (`confident` / `verified` /
    /// `unverifiable` / `rejected` — see `tt_telemetry::l2_receipt`). None for
    /// non-L2 rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l2_verdict: Option<String>,
}

/// `skip_serializing_if` helper: the minify estimate column is omitted from
/// serialized rows when zero, keeping pre-0020 row JSON byte-identical.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn f64_is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// Errors returned by [`RequestLogWriter`].
#[derive(Debug, Error)]
pub enum RequestLogError {
    /// A TERMINAL storage error: a unique-key violation (the row already
    /// committed — see the idempotency note on [`write_with_retry`]), a
    /// constraint/check failure, or any other error that retrying cannot fix.
    /// Retrying these is pointless, so [`RequestLogError::is_transient`]
    /// returns `false`.
    #[error("storage: {0}")]
    Storage(String),
    /// A TRANSIENT storage error worth retrying: a pool-acquire timeout, a
    /// dropped/closed connection, or a low-level I/O error. The committed row
    /// is billable revenue, so the writer retries these a bounded number of
    /// times (see [`write_with_retry`]) before giving up.
    #[error("transient storage: {0}")]
    Transient(String),
}

impl RequestLogError {
    /// `true` for errors [`write_with_retry`] should re-attempt (connection /
    /// pool / I/O blips). Terminal errors (unique-key violation, constraint
    /// failure) return `false` — retrying them would either re-hit the same
    /// committed row's PK or fail identically.
    pub fn is_transient(&self) -> bool {
        matches!(self, RequestLogError::Transient(_))
    }
}

/// Persistence contract for the `request_logs` table.
#[async_trait]
pub trait RequestLogWriter: Send + Sync {
    async fn write(&self, row: RequestLogRow) -> Result<(), RequestLogError>;
}

/// Default bounded retry budget for [`write_with_retry`] — the total number of
/// write attempts (1 initial + 2 retries). A `request_logs` row is billable
/// revenue, so a transient DB blip must not silently drop it.
pub const DEFAULT_WRITE_ATTEMPTS: u32 = 3;

/// Base backoff between retry attempts in [`write_with_retry`]. Doubled each
/// attempt (50ms, 100ms). Deliberately short: the spawned write task is best-
/// effort and may be drained on shutdown, so retries must not pin a task for
/// long.
pub const DEFAULT_WRITE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

/// Write a `request_logs` row with a bounded retry on TRANSIENT failure.
///
/// Revenue substrate: a committed-but-unwritten billing row is silent
/// under-billing. So a transient storage error (connection / pool / I/O blip,
/// classified by [`RequestLogError::is_transient`]) is retried up to
/// `max_attempts` total tries with an exponentially-growing backoff starting
/// at `base_backoff`. A TERMINAL error (`Storage`) is returned immediately —
/// retrying it cannot help.
///
/// **Idempotent-safe.** Every retry re-sends the SAME `row` (same `id`, the
/// `request_logs` PRIMARY KEY). If the original INSERT never committed, the
/// retry inserts the single row. If it DID commit but the ack was lost, the
/// retry hits the PK unique-violation — which the Postgres writer surfaces as a
/// TERMINAL `Storage` error (not `Transient`), so the loop stops with no
/// duplicate row. Either way the table ends with exactly one row per `id`.
///
/// Returns the outcome of the final attempt; the caller is responsible for
/// counting a permanent failure (e.g. `tt_request_log_write_failed_total`).
pub async fn write_with_retry<W: RequestLogWriter + ?Sized>(
    writer: &W,
    row: RequestLogRow,
    max_attempts: u32,
    base_backoff: std::time::Duration,
) -> Result<(), RequestLogError> {
    let attempts = max_attempts.max(1);
    let mut backoff = base_backoff;
    for attempt in 1..=attempts {
        // Clone per attempt so a retry re-sends the identical row (same PK).
        match writer.write(row.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let last = attempt == attempts;
                if last || !e.is_transient() {
                    return Err(e);
                }
                tracing::warn!(
                    error = %e,
                    attempt,
                    max_attempts = attempts,
                    "request_logs write transient failure — retrying"
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2);
            }
        }
    }
    // Unreachable: the loop always returns on the final attempt.
    Ok(())
}

// ─── InMemory ──────────────────────────────────────────────────────────────

/// Process-local writer for tests. Stores rows in a `Vec` accessible
/// via [`InMemoryRequestLogWriter::rows`].
#[derive(Clone, Default)]
pub struct InMemoryRequestLogWriter {
    inner: Arc<Mutex<Vec<RequestLogRow>>>,
}

impl InMemoryRequestLogWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of all rows captured so far.
    pub fn rows(&self) -> Vec<RequestLogRow> {
        self.inner
            .lock()
            .expect("request log writer mutex poisoned")
            .clone()
    }
}

#[async_trait]
impl RequestLogWriter for InMemoryRequestLogWriter {
    async fn write(&self, row: RequestLogRow) -> Result<(), RequestLogError> {
        self.inner
            .lock()
            .map_err(|e| RequestLogError::Storage(e.to_string()))?
            .push(row);
        Ok(())
    }
}

// ─── Postgres ──────────────────────────────────────────────────────────────

/// Postgres-backed writer, gated by the `postgres` feature.
#[cfg(feature = "postgres")]
pub mod postgres {
    use super::*;
    use sqlx::PgPool;

    /// Production writer. INSERTs into `request_logs`.
    #[derive(Clone)]
    pub struct PostgresRequestLogWriter {
        pool: PgPool,
    }

    impl PostgresRequestLogWriter {
        pub fn new(pool: PgPool) -> Self {
            Self { pool }
        }
    }

    impl std::fmt::Debug for PostgresRequestLogWriter {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PostgresRequestLogWriter")
                .field("pool", &"PgPool { .. }")
                .finish()
        }
    }

    /// INSERT SQL for `request_logs`. Exposed as a constant so the column /
    /// placeholder / bind counts can be verified in tests without duplicating
    /// the string.
    pub const INSERT_SQL: &str = r#"INSERT INTO request_logs
                     (id, org_id, api_key_id, ts, provider, model,
                      input_tokens, output_tokens, cached_tokens,
                      cost_usd, baseline_cost_usd, provider_cache_saved_usd,
                      cache_bust_penalty_usd,
                      cached, cache_layer,
                      route_id, latency_ms, upstream_latency_ms, status,
                      tag, error_class, trace_id,
                      truncated,
                      shadow_model, shadow_cost_usd, traffic_split_arm,
                      cache_read_input_tokens, cache_creation_input_tokens,
                      batch_eligible, batch_forgone_usd,
                      route_paused,
                      minify_saved_est_usd,
                      format_switched, format_switch_saved_est_usd,
                      diff_applied, diff_saved_usd,
                      diff_failed, diff_failed_cost_usd,
                      retrieval_tokens_saved,
                      run_id, node_id,
                      doc_compaction_tokens_removed,
                      compression_saved_usd, compression_tokens_removed,
                      doc_vision_saved_est_usd,
                      content_compress_saved_est_usd, content_compress_kind,
                      l2_matched_entry_id, l2_similarity, l2_verdict)
                   VALUES
                     ($1, $2, $3, $4, $5, $6,
                      $7, $8, $9,
                      $10, $11, $12,
                      $13,
                      $14, $15,
                      $16, $17, $18, $19,
                      $20, $21, $22,
                      $23,
                      $24, $25, $26,
                      $27, $28,
                      $29, $30,
                      $31,
                      $32,
                      $33, $34,
                      $35, $36,
                      $37, $38,
                      $39,
                      $40, $41,
                      $42,
                      $43, $44,
                      $45,
                      $46, $47,
                      $48, $49, $50)"#;

    /// Number of `.bind(...)` calls in [`PostgresRequestLogWriter::write`].
    /// Must stay in sync with [`INSERT_SQL`] and the actual bind chain.
    pub const INSERT_BIND_COUNT: usize = 50;

    #[async_trait]
    impl RequestLogWriter for PostgresRequestLogWriter {
        async fn write(&self, row: RequestLogRow) -> Result<(), RequestLogError> {
            sqlx::query(INSERT_SQL)
                .bind(row.id) // $1
                .bind(row.org_id) // $2
                .bind(row.api_key_id) // $3
                .bind(row.ts) // $4
                .bind(&row.provider) // $5
                .bind(&row.model) // $6
                .bind(row.input_tokens) // $7
                .bind(row.output_tokens) // $8
                .bind(row.cached_tokens) // $9
                .bind(row.cost_usd) // $10
                .bind(row.baseline_cost_usd) // $11
                .bind(row.provider_cache_saved_usd) // $12
                .bind(row.cache_bust_penalty_usd) // $13
                .bind(row.cached) // $14
                .bind(row.cache_layer.as_deref()) // $15
                .bind(row.route_id) // $16
                .bind(row.latency_ms) // $17
                .bind(row.upstream_latency_ms) // $18
                .bind(row.status) // $19
                .bind(row.tag.as_deref()) // $20
                .bind(row.error_class.as_deref()) // $21
                .bind(row.trace_id.as_deref()) // $22
                .bind(row.truncated) // $23
                .bind(row.shadow_model.as_deref()) // $24
                .bind(row.shadow_cost_usd) // $25
                .bind(row.traffic_split_arm.as_deref()) // $26
                .bind(row.cache_read_input_tokens) // $27
                .bind(row.cache_creation_input_tokens) // $28
                .bind(row.batch_eligible) // $29
                .bind(row.batch_forgone_usd) // $30
                .bind(row.route_paused) // $31
                .bind(row.minify_saved_est_usd) // $32
                .bind(row.format_switched.as_deref()) // $33
                .bind(row.format_switch_saved_est_usd) // $34
                .bind(row.diff_applied) // $35
                .bind(row.diff_saved_usd) // $36
                .bind(row.diff_failed) // $37
                .bind(row.diff_failed_cost_usd) // $38
                .bind(row.retrieval_tokens_saved) // $39
                .bind(row.run_id) // $40
                .bind(row.node_id) // $41
                .bind(row.doc_compaction_tokens_removed) // $42
                .bind(row.compression_saved_usd) // $43
                .bind(row.compression_tokens_removed) // $44
                .bind(row.doc_vision_saved_est_usd) // $45
                .bind(row.content_compress_saved_est_usd) // $46
                .bind(row.content_compress_kind.as_deref()) // $47
                .bind(row.l2_matched_entry_id) // $48
                .bind(row.l2_similarity) // $49
                .bind(row.l2_verdict.as_deref()) // $50
                .execute(&self.pool)
                .await
                .map_err(classify_sqlx_error)?;
            Ok(())
        }
    }

    /// Map a `sqlx::Error` to a [`RequestLogError`], splitting TRANSIENT blips
    /// (worth retrying) from TERMINAL failures (not).
    ///
    /// TRANSIENT: pool-acquire timeout, a closed/dropped connection, or a
    /// low-level I/O error — the row never committed and a retry on a fresh
    /// pooled connection can succeed.
    ///
    /// TERMINAL: a `Database` error (this includes a unique-key violation on
    /// the `id` PRIMARY KEY — proof the row ALREADY committed, so a retry must
    /// NOT re-insert — and any constraint/check failure), plus encode/decode
    /// and other non-recoverable errors.
    fn classify_sqlx_error(e: sqlx::Error) -> RequestLogError {
        let transient = matches!(
            e,
            sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_)
        ) || matches!(&e, sqlx::Error::Tls(_));
        if transient {
            RequestLogError::Transient(e.to_string())
        } else {
            RequestLogError::Storage(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test writer that returns the queued error/ok results in order, recording
    /// how many `write` calls it saw and the `id` of every row it was handed.
    /// Drives the [`write_with_retry`] retry-budget tests.
    struct ScriptedWriter {
        /// Per-attempt outcome: `None` = Ok, `Some(true)` = Transient error,
        /// `Some(false)` = terminal Storage error. Once exhausted, returns Ok.
        script: Mutex<std::collections::VecDeque<Option<bool>>>,
        calls: AtomicUsize,
        ids: Mutex<Vec<Uuid>>,
    }

    impl ScriptedWriter {
        fn new(script: Vec<Option<bool>>) -> Self {
            Self {
                script: Mutex::new(script.into_iter().collect()),
                calls: AtomicUsize::new(0),
                ids: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl RequestLogWriter for ScriptedWriter {
        async fn write(&self, row: RequestLogRow) -> Result<(), RequestLogError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.ids.lock().unwrap().push(row.id);
            let next = self.script.lock().unwrap().pop_front().flatten();
            match next {
                None => Ok(()),
                Some(true) => Err(RequestLogError::Transient("connection reset".into())),
                Some(false) => Err(RequestLogError::Storage("duplicate key".into())),
            }
        }
    }

    fn sample_row() -> RequestLogRow {
        RequestLogRow {
            id: Uuid::now_v7(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            ts: Utc::now(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            input_tokens: 100,
            output_tokens: 50,
            cached_tokens: 0,
            cost_usd: 0.0045,
            baseline_cost_usd: 0.0045,
            provider_cache_saved_usd: 0.0,
            cache_bust_penalty_usd: 0.0,
            cached: false,
            cache_layer: None,
            route_id: None,
            latency_ms: 800,
            upstream_latency_ms: Some(750),
            status: 200,
            tag: None,
            error_class: None,
            trace_id: Some("trace-abc".into()),
            truncated: false,
            shadow_model: None,
            shadow_cost_usd: None,
            traffic_split_arm: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            batch_eligible: false,
            batch_forgone_usd: 0.0,
            route_paused: false,
            minify_saved_est_usd: 0.0,
            format_switched: None,
            format_switch_saved_est_usd: 0.0,
            diff_applied: false,
            diff_saved_usd: 0.0,
            diff_failed: false,
            diff_failed_cost_usd: 0.0,
            retrieval_tokens_saved: 0,
            doc_compaction_tokens_removed: 0,
            compression_saved_usd: 0.0,
            compression_tokens_removed: 0,
            doc_vision_saved_est_usd: 0.0,
            run_id: None,
            node_id: None,
            content_compress_saved_est_usd: 0.0,
            content_compress_kind: None,
            l2_matched_entry_id: None,
            l2_similarity: None,
            l2_verdict: None,
        }
    }

    #[tokio::test]
    async fn in_memory_collects_rows() {
        let w = InMemoryRequestLogWriter::new();
        w.write(sample_row()).await.unwrap();
        w.write(sample_row()).await.unwrap();
        assert_eq!(w.rows().len(), 2);
    }

    // ── write_with_retry (P2 metering durability) ──────────────────────────────

    /// A transient blip that resolves on the 2nd attempt: `write_with_retry`
    /// retries and ultimately succeeds. Uses a zero backoff so the test is fast.
    #[tokio::test]
    async fn retry_recovers_from_transient_then_succeeds() {
        // attempt 1 → Transient, attempt 2 → Ok.
        let w = ScriptedWriter::new(vec![Some(true), None]);
        let res = write_with_retry(&w, sample_row(), 3, std::time::Duration::ZERO).await;
        assert!(res.is_ok(), "transient-then-ok must succeed: {res:?}");
        assert_eq!(w.calls(), 2, "should retry exactly once after the blip");
    }

    /// Retries are bounded: a writer that is transiently broken for MORE
    /// attempts than the budget gives up and returns the transient error after
    /// exactly `max_attempts` tries (no infinite loop).
    #[tokio::test]
    async fn retry_is_bounded_and_returns_last_error() {
        // Always transient.
        let w = ScriptedWriter::new(vec![Some(true), Some(true), Some(true), Some(true)]);
        let res = write_with_retry(&w, sample_row(), 3, std::time::Duration::ZERO).await;
        let err = res.expect_err("exhausted transient retries must return Err");
        assert!(
            err.is_transient(),
            "the surfaced error is the transient one"
        );
        assert_eq!(w.calls(), 3, "must stop at exactly max_attempts");
    }

    /// A TERMINAL (`Storage`) error is NOT retried — retrying a unique-key
    /// violation (the row already committed) would be wasted work and could
    /// not succeed. Fails fast after a single attempt.
    #[tokio::test]
    async fn retry_does_not_retry_terminal_error() {
        // attempt 1 → terminal Storage error.
        let w = ScriptedWriter::new(vec![Some(false), None]);
        let res = write_with_retry(&w, sample_row(), 3, std::time::Duration::ZERO).await;
        let err = res.expect_err("terminal error must surface");
        assert!(!err.is_transient(), "Storage is terminal, not transient");
        assert_eq!(w.calls(), 1, "terminal error must NOT be retried");
    }

    /// Idempotency: every retry re-sends the SAME row `id` (the `request_logs`
    /// PRIMARY KEY), so a retried INSERT cannot create a duplicate row.
    #[tokio::test]
    async fn retry_resends_identical_row_id() {
        let w = ScriptedWriter::new(vec![Some(true), Some(true), None]);
        let row = sample_row();
        let id = row.id;
        write_with_retry(&w, row, 3, std::time::Duration::ZERO)
            .await
            .unwrap();
        let ids = w.ids.lock().unwrap();
        assert_eq!(ids.len(), 3, "three attempts");
        assert!(
            ids.iter().all(|&seen| seen == id),
            "every retry must carry the same PK id ({id}), got {ids:?}"
        );
    }

    /// `is_transient` correctly partitions the two variants.
    #[test]
    fn error_transient_classification() {
        assert!(RequestLogError::Transient("pool timeout".into()).is_transient());
        assert!(!RequestLogError::Storage("unique violation".into()).is_transient());
    }

    #[tokio::test]
    async fn in_memory_round_trips_field_values() {
        let w = InMemoryRequestLogWriter::new();
        let row = sample_row();
        let id = row.id;
        let trace = row.trace_id.clone();
        w.write(row).await.unwrap();
        let captured = &w.rows()[0];
        assert_eq!(captured.id, id);
        assert_eq!(captured.trace_id, trace);
        assert_eq!(captured.provider, "openai");
        assert!((captured.cost_usd - 0.0045).abs() < 1e-9);
    }

    /// Persist a row with `truncated = true` and verify it round-trips through
    /// the in-memory writer.
    #[tokio::test]
    async fn in_memory_round_trips_truncated_true() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.truncated = true;
        w.write(row).await.unwrap();
        assert!(
            w.rows()[0].truncated,
            "truncated=true must survive write→read"
        );
    }

    /// A row carrying the canary columns (shadow_model / shadow_cost_usd /
    /// traffic_split_arm) round-trips through the writer with the shadow cost in
    /// its OWN field — never folded into `cost_usd`.
    #[tokio::test]
    async fn in_memory_round_trips_canary_columns() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cost_usd = 0.01;
        row.shadow_model = Some("claude-haiku-4-5".into());
        row.shadow_cost_usd = Some(0.004);
        row.traffic_split_arm = Some("canary".into());
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert_eq!(got.shadow_model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(got.shadow_cost_usd, Some(0.004));
        assert_eq!(got.traffic_split_arm.as_deref(), Some("canary"));
        // The shadow cost is SEPARATE — the primary cost is untouched.
        assert!((got.cost_usd - 0.01).abs() < 1e-9);
    }

    /// A row carrying the provider prompt-cache token columns round-trips, and
    /// `Some(0)` ("provider explicitly reported zero") stays distinct from
    /// `None` ("provider did not report" / "no provider call").
    #[tokio::test]
    async fn in_memory_round_trips_provider_cache_token_columns() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cache_read_input_tokens = Some(80);
        row.cache_creation_input_tokens = Some(20);
        w.write(row).await.unwrap();

        let mut zero = sample_row();
        zero.cache_read_input_tokens = Some(0);
        zero.cache_creation_input_tokens = None;
        w.write(zero).await.unwrap();

        let rows = w.rows();
        assert_eq!(rows[0].cache_read_input_tokens, Some(80));
        assert_eq!(rows[0].cache_creation_input_tokens, Some(20));
        // NULL-vs-0: a reported zero survives as Some(0), not None.
        assert_eq!(rows[1].cache_read_input_tokens, Some(0));
        assert_eq!(rows[1].cache_creation_input_tokens, None);
    }

    /// Legacy JSON (rows serialized before migration 0015) deserializes with
    /// both provider cache token fields defaulting to `None`, and `None`
    /// fields are omitted on re-serialize.
    #[test]
    fn provider_cache_token_columns_serde_backward_compat() {
        let legacy = r#"{
            "id":"00000000-0000-0000-0000-000000000000",
            "org_id":"00000000-0000-0000-0000-000000000000",
            "api_key_id":"00000000-0000-0000-0000-000000000000",
            "ts":"2026-06-09T00:00:00Z",
            "provider":"openai","model":"gpt-4o",
            "input_tokens":1,"output_tokens":1,"cached_tokens":0,
            "cost_usd":0.0,"baseline_cost_usd":0.0,
            "cached":false,"cache_layer":null,"route_id":null,
            "latency_ms":1,"upstream_latency_ms":null,"status":200,
            "tag":null,"error_class":null,"trace_id":null
        }"#;
        let row: RequestLogRow = serde_json::from_str(legacy).unwrap();
        assert_eq!(row.cache_read_input_tokens, None);
        assert_eq!(row.cache_creation_input_tokens, None);
        let j = serde_json::to_string(&row).unwrap();
        assert!(!j.contains("cache_read_input_tokens"), "{j}");
        assert!(!j.contains("cache_creation_input_tokens"), "{j}");
    }

    /// Legacy JSON (rows serialized before the canary columns existed)
    /// deserializes with all three new fields defaulting to `None`, and a
    /// `None`-valued row omits them on re-serialize (back-compat with old
    /// deploys / persisted rows).
    #[test]
    fn canary_columns_serde_backward_compat() {
        let legacy = r#"{
            "id":"00000000-0000-0000-0000-000000000000",
            "org_id":"00000000-0000-0000-0000-000000000000",
            "api_key_id":"00000000-0000-0000-0000-000000000000",
            "ts":"2026-06-09T00:00:00Z",
            "provider":"openai","model":"gpt-4o",
            "input_tokens":1,"output_tokens":1,"cached_tokens":0,
            "cost_usd":0.0,"baseline_cost_usd":0.0,
            "cached":false,"cache_layer":null,"route_id":null,
            "latency_ms":1,"upstream_latency_ms":null,"status":200,
            "tag":null,"error_class":null,"trace_id":null
        }"#;
        let row: RequestLogRow = serde_json::from_str(legacy).unwrap();
        assert_eq!(row.shadow_model, None);
        assert_eq!(row.shadow_cost_usd, None);
        assert_eq!(row.traffic_split_arm, None);
        // Pre-0016 rows default the bust penalty to 0 (no bust booked).
        assert_eq!(row.cache_bust_penalty_usd, 0.0);
        let j = serde_json::to_string(&row).unwrap();
        assert!(!j.contains("shadow_model"), "{j}");
        assert!(!j.contains("shadow_cost_usd"), "{j}");
        assert!(!j.contains("traffic_split_arm"), "{j}");
    }

    /// Legacy JSON (rows serialized before migration 0019) deserializes with
    /// `route_paused` defaulting to false, and a `route_paused = true` row
    /// round-trips through the in-memory writer (mirror of `truncated`).
    #[tokio::test]
    async fn route_paused_serde_backward_compat() {
        let legacy = r#"{
            "id":"00000000-0000-0000-0000-000000000000",
            "org_id":"00000000-0000-0000-0000-000000000000",
            "api_key_id":"00000000-0000-0000-0000-000000000000",
            "ts":"2026-06-09T00:00:00Z",
            "provider":"openai","model":"gpt-4o",
            "input_tokens":1,"output_tokens":1,"cached_tokens":0,
            "cost_usd":0.0,"baseline_cost_usd":0.0,
            "cached":false,"cache_layer":null,"route_id":null,
            "latency_ms":1,"upstream_latency_ms":null,"status":200,
            "tag":null,"error_class":null,"trace_id":null
        }"#;
        let row: RequestLogRow = serde_json::from_str(legacy).unwrap();
        assert!(
            !row.route_paused,
            "pre-0019 rows must deserialize route_paused = false"
        );

        let w = InMemoryRequestLogWriter::new();
        let mut paused_row = sample_row();
        paused_row.route_paused = true;
        w.write(paused_row).await.unwrap();
        assert!(
            w.rows()[0].route_paused,
            "route_paused=true must survive write→read"
        );
    }

    /// The cache-bust penalty column (migration 0016) round-trips through the
    /// writer in its OWN field — never folded into `cost_usd` (it is an
    /// estimate of induced future cost, not a realized invoice figure).
    #[tokio::test]
    async fn in_memory_round_trips_cache_bust_penalty() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cost_usd = 0.01;
        row.cache_bust_penalty_usd = 0.0025;
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert!((got.cache_bust_penalty_usd - 0.0025).abs() < 1e-12);
        assert!((got.cost_usd - 0.01).abs() < 1e-12, "cost untouched");
    }

    /// The batch-eligibility columns (migration 0017) round-trip through the
    /// writer in their OWN fields — the forgone discount is a projection for
    /// the future async Batch Lane and is never folded into `cost_usd`.
    #[tokio::test]
    async fn in_memory_round_trips_batch_columns() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cost_usd = 0.025;
        row.batch_eligible = true;
        row.batch_forgone_usd = 0.0125;
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert!(got.batch_eligible, "batch_eligible must survive write→read");
        assert!((got.batch_forgone_usd - 0.0125).abs() < 1e-12);
        assert!((got.cost_usd - 0.025).abs() < 1e-12, "cost untouched");
    }

    /// Legacy JSON (rows serialized before migration 0017) deserializes with
    /// `batch_eligible = false` / `batch_forgone_usd = 0.0` — matching the SQL
    /// `NOT NULL DEFAULT` semantics for pre-migration rows.
    #[test]
    fn batch_columns_serde_backward_compat() {
        let legacy = r#"{
            "id":"00000000-0000-0000-0000-000000000000",
            "org_id":"00000000-0000-0000-0000-000000000000",
            "api_key_id":"00000000-0000-0000-0000-000000000000",
            "ts":"2026-06-09T00:00:00Z",
            "provider":"openai","model":"gpt-4o",
            "input_tokens":1,"output_tokens":1,"cached_tokens":0,
            "cost_usd":0.0,"baseline_cost_usd":0.0,
            "cached":false,"cache_layer":null,"route_id":null,
            "latency_ms":1,"upstream_latency_ms":null,"status":200,
            "tag":null,"error_class":null,"trace_id":null
        }"#;
        let row: RequestLogRow = serde_json::from_str(legacy).unwrap();
        assert!(!row.batch_eligible, "pre-0017 rows default to ineligible");
        assert_eq!(row.batch_forgone_usd, 0.0, "pre-0017 rows forgo nothing");
    }

    /// The minify estimate column (migration 0020) round-trips through the
    /// writer in its OWN field — an ESTIMATE of an unmeasurable counterfactual,
    /// never folded into `cost_usd`.
    #[tokio::test]
    async fn in_memory_round_trips_minify_estimate() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cost_usd = 0.02;
        row.minify_saved_est_usd = 0.0031;
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert!((got.minify_saved_est_usd - 0.0031).abs() < 1e-12);
        assert!((got.cost_usd - 0.02).abs() < 1e-12, "cost untouched");
    }

    /// Document Lane D4: the isolated vision-avoided estimate column (migration
    /// 0032) round-trips in its OWN field, defaults to 0.0, and is serde-omitted
    /// when 0.0 so legacy row JSON stays byte-identical (mirror of the minify
    /// estimate).
    #[tokio::test]
    async fn in_memory_round_trips_doc_vision_estimate() {
        // Defaults to 0.0 and omitted from JSON when 0.0.
        assert_eq!(sample_row().doc_vision_saved_est_usd, 0.0);
        let zero_json = serde_json::to_string(&sample_row()).unwrap();
        assert!(
            !zero_json.contains("doc_vision_saved_est_usd"),
            "zero must be omitted on serialize: {zero_json}"
        );

        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cost_usd = 0.02;
        row.doc_vision_saved_est_usd = 0.0031;
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert!((got.doc_vision_saved_est_usd - 0.0031).abs() < 1e-12);
        assert!((got.cost_usd - 0.02).abs() < 1e-12, "cost untouched");

        // A non-zero value is present on serialize.
        let mut row2 = sample_row();
        row2.doc_vision_saved_est_usd = 0.001;
        let j2 = serde_json::to_string(&row2).unwrap();
        assert!(j2.contains("doc_vision_saved_est_usd"), "{j2}");

        // A legacy row that omits the column deserializes to 0.0.
        let json = r#"{"id":"00000000-0000-0000-0000-000000000000","org_id":"00000000-0000-0000-0000-000000000000","api_key_id":"00000000-0000-0000-0000-000000000000","ts":"2026-06-30T00:00:00Z","provider":"p","model":"m","input_tokens":1,"output_tokens":1,"cached_tokens":0,"cost_usd":0.0,"baseline_cost_usd":0.0,"provider_cache_saved_usd":0.0,"cache_bust_penalty_usd":0.0,"cached":false,"route_id":null,"latency_ms":1,"upstream_latency_ms":null,"status":200,"tag":null,"error_class":null,"trace_id":null,"truncated":false}"#;
        let legacy: RequestLogRow = serde_json::from_str(json).unwrap();
        assert_eq!(
            legacy.doc_vision_saved_est_usd, 0.0,
            "legacy rows default to 0"
        );
    }

    /// Content-aware compression (P1a, migration 0033): the isolated estimated
    /// saving round-trips in its OWN field, defaults to 0.0, is serde-omitted
    /// when 0.0 (legacy JSON byte-identical), and the `content_compress_kind`
    /// flywheel label round-trips + defaults to None + is omitted when None.
    #[tokio::test]
    async fn in_memory_round_trips_content_compress_columns() {
        // Defaults + zero/None omitted on serialize.
        assert_eq!(sample_row().content_compress_saved_est_usd, 0.0);
        assert_eq!(sample_row().content_compress_kind, None);
        let zero_json = serde_json::to_string(&sample_row()).unwrap();
        assert!(
            !zero_json.contains("content_compress_saved_est_usd"),
            "zero saving must be omitted on serialize: {zero_json}"
        );
        assert!(
            !zero_json.contains("content_compress_kind"),
            "None kind must be omitted on serialize: {zero_json}"
        );

        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cost_usd = 0.02;
        row.content_compress_saved_est_usd = 0.0031;
        row.content_compress_kind = Some("json".into());
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert!((got.content_compress_saved_est_usd - 0.0031).abs() < 1e-12);
        assert_eq!(got.content_compress_kind.as_deref(), Some("json"));
        assert!((got.cost_usd - 0.02).abs() < 1e-12, "cost untouched");

        // Non-zero / Some are present on serialize.
        let mut row2 = sample_row();
        row2.content_compress_saved_est_usd = 0.001;
        row2.content_compress_kind = Some("log".into());
        let j2 = serde_json::to_string(&row2).unwrap();
        assert!(j2.contains("content_compress_saved_est_usd"), "{j2}");
        assert!(j2.contains("content_compress_kind"), "{j2}");

        // A legacy row that omits the columns deserializes to 0.0 / None.
        let json = r#"{"id":"00000000-0000-0000-0000-000000000000","org_id":"00000000-0000-0000-0000-000000000000","api_key_id":"00000000-0000-0000-0000-000000000000","ts":"2026-06-30T00:00:00Z","provider":"p","model":"m","input_tokens":1,"output_tokens":1,"cached_tokens":0,"cost_usd":0.0,"baseline_cost_usd":0.0,"provider_cache_saved_usd":0.0,"cache_bust_penalty_usd":0.0,"cached":false,"route_id":null,"latency_ms":1,"upstream_latency_ms":null,"status":200,"tag":null,"error_class":null,"trace_id":null,"truncated":false}"#;
        let legacy: RequestLogRow = serde_json::from_str(json).unwrap();
        assert_eq!(legacy.content_compress_saved_est_usd, 0.0);
        assert_eq!(legacy.content_compress_kind, None);
    }

    /// The output-shaping columns (migration 0020) round-trip through the
    /// writer in their OWN fields. `format_switch_saved_est_usd` is a LABELED
    /// ESTIMATE and `diff_failed_cost_usd` duplicates real spend already in
    /// `cost_usd` — neither mutates any other figure on write.
    #[tokio::test]
    async fn in_memory_round_trips_output_shaping_columns() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cost_usd = 0.03;
        row.format_switched = Some("csv".into());
        row.format_switch_saved_est_usd = 0.002;
        row.diff_applied = true;
        row.diff_saved_usd = 0.015;
        row.diff_failed = true;
        row.diff_failed_cost_usd = 0.004;
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert_eq!(got.format_switched.as_deref(), Some("csv"));
        assert!((got.format_switch_saved_est_usd - 0.002).abs() < 1e-12);
        assert!(got.diff_applied);
        assert!((got.diff_saved_usd - 0.015).abs() < 1e-12);
        assert!(got.diff_failed);
        assert!((got.diff_failed_cost_usd - 0.004).abs() < 1e-12);
        assert!((got.cost_usd - 0.03).abs() < 1e-12, "cost untouched");
    }

    /// Legacy JSON (rows serialized before migration 0020) deserializes with
    /// all six output-shaping fields at their SQL defaults (NULL/0/false), and
    /// a default-valued row omits `format_switched` on re-serialize.
    #[test]
    fn output_shaping_columns_serde_backward_compat() {
        let legacy = r#"{
            "id":"00000000-0000-0000-0000-000000000000",
            "org_id":"00000000-0000-0000-0000-000000000000",
            "api_key_id":"00000000-0000-0000-0000-000000000000",
            "ts":"2026-06-09T00:00:00Z",
            "provider":"openai","model":"gpt-4o",
            "input_tokens":1,"output_tokens":1,"cached_tokens":0,
            "cost_usd":0.0,"baseline_cost_usd":0.0,
            "cached":false,"cache_layer":null,"route_id":null,
            "latency_ms":1,"upstream_latency_ms":null,"status":200,
            "tag":null,"error_class":null,"trace_id":null
        }"#;
        let row: RequestLogRow = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            row.minify_saved_est_usd, 0.0,
            "pre-0020 rows must default to 0.0 (nothing estimated)"
        );
        let j = serde_json::to_string(&row).unwrap();
        assert!(
            !j.contains("minify_saved_est_usd"),
            "zero estimate must be omitted on serialize: {j}"
        );
        // A nonzero estimate IS serialized.
        let mut row2 = sample_row();
        row2.minify_saved_est_usd = 0.001;
        let j2 = serde_json::to_string(&row2).unwrap();
        assert!(j2.contains("minify_saved_est_usd"), "{j2}");
        assert_eq!(row.format_switched, None);
        assert_eq!(row.format_switch_saved_est_usd, 0.0);
        assert!(!row.diff_applied);
        assert_eq!(row.diff_saved_usd, 0.0);
        assert!(!row.diff_failed);
        assert_eq!(row.diff_failed_cost_usd, 0.0);
        let j = serde_json::to_string(&row).unwrap();
        assert!(!j.contains("format_switched"), "{j}");
    }

    /// Back-compat: legacy JSON (rows serialized before W0b migration 0027)
    /// deserializes with both `run_id` and `node_id` defaulting to `None`.
    /// The new fields must carry `#[serde(default)]` so no existing persisted
    /// row breaks on deserialization.
    #[test]
    fn request_log_row_carries_run_and_node_id() {
        let json = r#"{"id":"00000000-0000-0000-0000-000000000000","org_id":"00000000-0000-0000-0000-000000000000","api_key_id":"00000000-0000-0000-0000-000000000000","ts":"2026-06-27T00:00:00Z","provider":"openai","model":"m","input_tokens":1,"output_tokens":1,"cached_tokens":0,"cost_usd":0.0,"baseline_cost_usd":0.0,"cached":false,"cache_layer":null,"route_id":null,"latency_ms":1,"upstream_latency_ms":null,"status":200,"tag":null,"error_class":null,"trace_id":null}"#;
        // Legacy row (no run_id/node_id) must still deserialize to None.
        let row: RequestLogRow =
            serde_json::from_str(json).expect("legacy RequestLogRow must deserialize");
        assert_eq!(row.run_id, None);
        assert_eq!(row.node_id, None);
    }

    /// Retrieval substitution accounting is token-denominated and can be
    /// negative. It round-trips independently from USD cost fields.
    #[tokio::test]
    async fn in_memory_round_trips_retrieval_tokens_saved() {
        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.cost_usd = 0.02;
        row.retrieval_tokens_saved = -7;
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert_eq!(got.retrieval_tokens_saved, -7);
        assert!((got.cost_usd - 0.02).abs() < 1e-12, "cost untouched");
    }

    /// Document Lane D2: the doc-compaction token count round-trips
    /// independently and defaults to 0 (mirror of `retrieval_tokens_saved`).
    #[tokio::test]
    async fn in_memory_round_trips_doc_compaction_tokens_removed() {
        // Defaults to 0.
        assert_eq!(sample_row().doc_compaction_tokens_removed, 0);

        let w = InMemoryRequestLogWriter::new();
        let mut row = sample_row();
        row.doc_compaction_tokens_removed = 4096;
        w.write(row).await.unwrap();
        let got = &w.rows()[0];
        assert_eq!(got.doc_compaction_tokens_removed, 4096);

        // A row that omits the column still deserializes (serde default).
        let json = r#"{"id":"00000000-0000-0000-0000-000000000000","org_id":"00000000-0000-0000-0000-000000000000","api_key_id":"00000000-0000-0000-0000-000000000000","ts":"2026-06-30T00:00:00Z","provider":"p","model":"m","input_tokens":1,"output_tokens":1,"cached_tokens":0,"cost_usd":0.0,"baseline_cost_usd":0.0,"provider_cache_saved_usd":0.0,"cache_bust_penalty_usd":0.0,"cached":false,"route_id":null,"latency_ms":1,"upstream_latency_ms":null,"status":200,"tag":null,"error_class":null,"trace_id":null,"truncated":false}"#;
        let legacy: RequestLogRow = serde_json::from_str(json).unwrap();
        assert_eq!(
            legacy.doc_compaction_tokens_removed, 0,
            "legacy rows default to 0"
        );
    }

    /// Guard: the INSERT_SQL column list, placeholder list, and the
    /// INSERT_BIND_COUNT constant must all agree.
    ///
    /// Parses INSERT_SQL to count:
    ///   • column names between the first `(` and its matching `)`
    ///   • `$N` placeholders in the VALUES `(...)` clause
    /// and asserts both equal INSERT_BIND_COUNT.
    ///
    /// If a future edit adds a column but forgets a placeholder (or vice versa)
    /// this test will catch it without needing a live Postgres connection.
    #[cfg(feature = "postgres")]
    #[test]
    fn insert_sql_column_placeholder_bind_counts_match() {
        use crate::request_logs::postgres::{INSERT_BIND_COUNT, INSERT_SQL};

        // --- count columns in the INSERT column list -------------------------
        // Find the text between the first '(' and its matching ')'.
        let col_start = INSERT_SQL.find('(').expect("INSERT_SQL must contain '('");
        let col_end = INSERT_SQL[col_start + 1..]
            .find(')')
            .expect("INSERT_SQL must contain closing ')'")
            + col_start
            + 1;
        let col_list = &INSERT_SQL[col_start + 1..col_end];
        let column_count = col_list.split(',').count();

        // --- count $N placeholders in the VALUES clause ----------------------
        // Every placeholder matches "$" followed by one or more digits.
        let placeholder_count = {
            let mut n = 0usize;
            let bytes = INSERT_SQL.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'$' {
                    let rest = &INSERT_SQL[i + 1..];
                    if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                        n += 1;
                    }
                }
                i += 1;
            }
            n
        };

        assert_eq!(
            column_count, INSERT_BIND_COUNT,
            "INSERT_SQL has {column_count} columns but INSERT_BIND_COUNT is {INSERT_BIND_COUNT}"
        );
        assert_eq!(
            placeholder_count, INSERT_BIND_COUNT,
            "INSERT_SQL has {placeholder_count} $N placeholders but INSERT_BIND_COUNT is {INSERT_BIND_COUNT}"
        );
    }

    /// Source-level guard: the ACTUAL `.bind(` chain in
    /// `PostgresRequestLogWriter::write` must contain exactly
    /// `INSERT_BIND_COUNT` calls. The SQL-parsing guard above cannot catch a
    /// duplicated/missing `.bind(...)` line (it only parses `INSERT_SQL`), and
    /// a bind-count mismatch fails EVERY production INSERT at runtime — this
    /// regression shipped once as a duplicated `.bind(row.route_paused)`
    /// (32 binds vs 31 placeholders). Cheap + hermetic: scans this file's
    /// source between the `sqlx::query(INSERT_SQL)` and `.execute(` markers.
    #[cfg(feature = "postgres")]
    #[test]
    fn write_bind_chain_call_count_matches_insert_bind_count() {
        use crate::request_logs::postgres::INSERT_BIND_COUNT;

        let source = include_str!("request_logs.rs");
        let start = source
            .find("sqlx::query(INSERT_SQL)")
            .expect("write() must build the query from INSERT_SQL");
        let rest = &source[start..];
        let end = rest
            .find(".execute(")
            .expect("write() must terminate the bind chain with .execute(");
        let chain = &rest[..end];
        let bind_calls = chain.matches(".bind(").count();
        assert_eq!(
            bind_calls, INSERT_BIND_COUNT,
            "PostgresRequestLogWriter::write has {bind_calls} .bind( calls but \
             INSERT_BIND_COUNT is {INSERT_BIND_COUNT} — a mismatch fails every \
             production request_logs INSERT"
        );
    }
}
