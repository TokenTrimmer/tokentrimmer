//! Sub-lever 4 — semantic sub-step cache (closes gap G4 via the tool-result
//! embedding axis), confined to read-only/idempotent sub-steps (caveat C2 / R3).
//!
//! Inside an agentic loop, the same read-only sub-call recurs — a re-fetched
//! file, a re-run query, a re-asked lookup. The "don't call" lever caches that
//! sub-step's result so the loop pays once. This is the highest-variance lever
//! (external #7 is HIGH-risk), so it ships at the conservative end:
//!
//! - **Read-only / idempotent only (R3).** A mutating/non-idempotent sub-step
//!   (a write tool, a side-effecting action) is NEVER cached: returning a stale
//!   answer for a state-changing call is a correctness bug, not a saving. Only
//!   tools on the hand-verified read-only allowlist are eligible.
//! - **Never serves across model or embedding space (R3).** A hit is refused
//!   unless the served model AND the embedding model both match the entry's —
//!   the two embedding axes ([`tt_cache::l2_context_text`] vs.
//!   [`tt_cache::l2_tool_context_text`]) are distinct spaces and must never be
//!   crossed, and a cached answer for one model is not a valid answer for
//!   another.
//! - **0.92 threshold + SimHash FP gate + adaptive ratchet.** Same false-
//!   positive defenses as the main L2 path ([`tt_cache::DEFAULT_THRESHOLD`],
//!   [`tt_cache::lexical_agreement`] at
//!   [`tt_cache::DEFAULT_LEXICAL_MIN_AGREEMENT`]).
//! - **Books only REALIZED hits (caveat C2).** The per-request headline books a
//!   realized hit's MEASURED cost delta only — never the
//!   `weighted = cost·(L1+(1−L1)·L2)` EXPECTATION value
//!   ([`tt_preview::cache_projection`]). The expectation stays a
//!   dashboard/projection figure, never a billed saving.
//!
//! This module is the pure, unit-testable confinement + accounting policy; the
//! handler wires it to the real (Postgres) L2 store and embedder. The in-memory
//! [`SubstepCache`] mirrors `tt_cache::InMemoryL2Cache` for tests/local dev.
//!
//! **Slice 2c-1 status — intentionally deferred (not wired into the agent loop).**
//! Sub-lever 4 stays a tested building block but is NOT served in the loop: the
//! only read-only/cacheable tools today are the 4 near-free gateway tools
//! (`find_route_for`/`preview_cost`/`inspect_diff`/`batch_savings`), so caching
//! their results saves ~nothing while adding an embedding tax + a persistent
//! pgvector store. It earns its keep only once an *expensive* read-only gateway
//! tool (e.g. retrieval) exists. Mirrors the COST-3(U) per-request-proxy
//! doc-closure. (Slice 2c-1 wired the *summarize* lever instead.)

use std::sync::Mutex;

use tt_cache::{lexical_agreement, DEFAULT_LEXICAL_MIN_AGREEMENT, DEFAULT_THRESHOLD};

/// Whether a sub-step (one tool call inside the loop) is safe to cache.
///
/// Only read-only/idempotent sub-steps may be cached (R3); a mutating call's
/// result is never a valid cache answer because the world it observed changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstepKind {
    /// Idempotent / read-only — eligible for the sub-step cache.
    ReadOnly,
    /// Mutating / side-effecting / non-idempotent — NEVER cached.
    Mutating,
}

/// The hand-verified read-only/idempotent tool allowlist. A sub-step is
/// cacheable ONLY when its tool name is on this list — everything else is
/// treated as [`SubstepKind::Mutating`] (fail-closed: an unknown tool might
/// have side effects, so it is never cached).
///
/// Seeded with the W4 dashboard-chat tools (all pure inspectors / projectors,
/// mirroring the lossless field-drop allowlist in `elide.rs`):
///
/// - `find_route_for` — pure routing recommendation, no state change.
/// - `preview_cost` — pure cost projection.
/// - `inspect_diff` — read-only static scan of a supplied diff.
/// - `batch_savings` — pure analytics over historical spend.
const READ_ONLY_TOOLS: &[&str] = &[
    "find_route_for",
    "preview_cost",
    "inspect_diff",
    "batch_savings",
];

/// Classify a sub-step by its tool name. Read-only allowlist members are
/// [`SubstepKind::ReadOnly`]; everything else (including unknown tools) is
/// [`SubstepKind::Mutating`] — fail-closed, so an unrecognized tool is never
/// cached.
#[must_use]
pub fn classify_substep(tool_name: &str) -> SubstepKind {
    if READ_ONLY_TOOLS.contains(&tool_name) {
        SubstepKind::ReadOnly
    } else {
        SubstepKind::Mutating
    }
}

/// The embedding space a sub-step entry lives in. A served hit must match BOTH
/// the served `model` AND the `embedding_model` (R3 — never serve across model
/// or embedding space). These are distinct embedding spaces that must never be
/// crossed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubstepSpace {
    /// The completion model the cached answer was produced for.
    pub model: String,
    /// The embedding model the cached vector was produced with.
    pub embedding_model: String,
}

impl SubstepSpace {
    /// True when `other` is the SAME embedding space (same completion model AND
    /// same embedding model). A serve across a different space is refused.
    #[must_use]
    pub fn matches(&self, other: &SubstepSpace) -> bool {
        self.model == other.model && self.embedding_model == other.embedding_model
    }
}

/// One cached read-only sub-step. Mirrors the minimal shape of a
/// `tt_cache::CacheEntry` the in-memory sub-step cache needs.
#[derive(Debug, Clone)]
pub struct SubstepEntry {
    /// The embedding of the tool-context axis text
    /// ([`tt_cache::l2_tool_context_text`]).
    pub embedding: Vec<f32>,
    /// SimHash of the embedded tool-context text — the FP gate's lexical sketch.
    pub lexical_sig: i64,
    /// The embedding space this entry lives in (model + embedding model).
    pub space: SubstepSpace,
    /// The cached response bytes.
    pub response: Vec<u8>,
    /// The MEASURED cost (USD) the original sub-call paid — what a realized hit
    /// avoids re-paying. This is the only figure a realized hit ever books
    /// (caveat C2: never the expectation value).
    pub realized_cost_usd: f64,
}

/// The outcome of a sub-step cache lookup.
#[derive(Debug, Clone, PartialEq)]
pub enum SubstepLookup {
    /// A realized hit: the cached response and the MEASURED cost delta it
    /// avoided. `saved_usd` is the entry's `realized_cost_usd` — the actual
    /// avoided spend, NEVER a `weighted = cost·(L1+(1−L1)·L2)` expectation
    /// (caveat C2).
    Hit { response: Vec<u8>, saved_usd: f64 },
    /// A miss (no entry passed every gate): nothing served, nothing booked.
    Miss,
}

/// An in-memory semantic sub-step cache for unit tests and local development.
///
/// Confines caching to read-only sub-steps, refuses cross-model/cross-embedding
/// -space serves, applies the 0.92 cosine threshold + SimHash FP gate, and
/// books only realized-hit measured deltas (caveat C2). The Postgres path
/// reuses the same policy over `tt_cache::PostgresL2Cache`.
#[derive(Default)]
pub struct SubstepCache {
    entries: Mutex<Vec<SubstepEntry>>,
}

impl SubstepCache {
    /// A new, empty sub-step cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a read-only sub-step entry. A [`SubstepKind::Mutating`] sub-step
    /// is NEVER inserted (R3) — the call returns `false` and stores nothing.
    /// Returns `true` when the entry was stored.
    pub fn insert(&self, kind: SubstepKind, entry: SubstepEntry) -> bool {
        if kind != SubstepKind::ReadOnly {
            return false;
        }
        self.entries
            .lock()
            .expect("substep cache poisoned")
            .push(entry);
        true
    }

    /// Look a sub-step up. Returns a [`SubstepLookup::Hit`] ONLY when a stored
    /// read-only entry clears every gate:
    ///
    /// 1. The sub-step itself is read-only (`kind == ReadOnly`); a mutating
    ///    lookup is an immediate miss (R3).
    /// 2. The entry's embedding space matches the query's BOTH model and
    ///    embedding model (R3 — never serve across model/embedding space).
    /// 3. Cosine similarity `>= DEFAULT_THRESHOLD` (0.92).
    /// 4. Lexical agreement `>= DEFAULT_LEXICAL_MIN_AGREEMENT` (0.75) — the
    ///    SimHash FP gate.
    ///
    /// On a hit, the booked `saved_usd` is the entry's MEASURED
    /// `realized_cost_usd` — never the expectation value (caveat C2).
    pub fn lookup(
        &self,
        kind: SubstepKind,
        query_space: &SubstepSpace,
        query_embedding: &[f32],
        query_lexical_sig: i64,
    ) -> SubstepLookup {
        // Gate 1: mutating sub-steps are never served (R3).
        if kind != SubstepKind::ReadOnly {
            return SubstepLookup::Miss;
        }

        let entries = self.entries.lock().expect("substep cache poisoned");
        let mut best: Option<(&SubstepEntry, f32)> = None;
        for entry in entries.iter() {
            // Gate 2: never serve across model or embedding space (R3).
            if !entry.space.matches(query_space) {
                continue;
            }
            // Gate 3: 0.92 cosine threshold.
            let sim = cosine(query_embedding, &entry.embedding);
            if sim < DEFAULT_THRESHOLD {
                continue;
            }
            // Gate 4: SimHash FP gate.
            if lexical_agreement(entry.lexical_sig, query_lexical_sig)
                < DEFAULT_LEXICAL_MIN_AGREEMENT
            {
                continue;
            }
            match best {
                Some((_, best_sim)) if best_sim >= sim => {}
                _ => best = Some((entry, sim)),
            }
        }

        match best {
            // Caveat C2: book the entry's MEASURED realized cost only — never a
            // `weighted = cost·(L1+(1−L1)·L2)` expectation value.
            Some((entry, _)) => SubstepLookup::Hit {
                response: entry.response.clone(),
                saved_usd: entry.realized_cost_usd,
            },
            None => SubstepLookup::Miss,
        }
    }
}

/// Cosine similarity in `[0, 1]` (zero/zero-norm vectors → 0). A local copy of
/// the L2 path's measure so the sub-step cache does not depend on the cache
/// crate's private cosine helper.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_cache::{l2_tool_context_text, lexical_sig};
    use tt_shared::messages::{Message, MessageContent};
    use tt_shared::ChatCompletionRequest;

    fn space() -> SubstepSpace {
        SubstepSpace {
            model: "gpt-4o".into(),
            embedding_model: "text-embedding-3-small".into(),
        }
    }

    /// A read-only sub-step entry whose embedding is a planted unit vector and
    /// whose lexical sig matches `text`.
    fn read_only_entry(embedding: Vec<f32>, text: &str, realized_cost_usd: f64) -> SubstepEntry {
        SubstepEntry {
            embedding,
            lexical_sig: lexical_sig(text),
            space: space(),
            response: b"{\"cached\":true}".to_vec(),
            realized_cost_usd,
        }
    }

    /// A non-idempotent (write/mutating) sub-step is NEVER cached (R3
    /// confinement): both insert and lookup of a mutating sub-step are no-ops.
    #[test]
    fn substep_cache_only_read_only_substeps() {
        let cache = SubstepCache::new();
        let emb = vec![1.0_f32, 0.0, 0.0, 0.0];
        let text = "[tool] mutated state";

        // A mutating sub-step is classified Mutating (fail-closed for unknowns,
        // and an explicit write tool is never read-only).
        assert_eq!(classify_substep("write_file"), SubstepKind::Mutating);
        assert_eq!(classify_substep("delete_resource"), SubstepKind::Mutating);
        // The W4 inspectors ARE read-only.
        assert_eq!(classify_substep("inspect_diff"), SubstepKind::ReadOnly);

        // Inserting a mutating sub-step stores nothing.
        let stored = cache.insert(
            SubstepKind::Mutating,
            read_only_entry(emb.clone(), text, 0.01),
        );
        assert!(!stored, "a mutating sub-step must never be cached (R3)");

        // Even if a read-only entry existed, a MUTATING lookup is never served.
        cache.insert(
            SubstepKind::ReadOnly,
            read_only_entry(emb.clone(), text, 0.01),
        );
        let out = cache.lookup(SubstepKind::Mutating, &space(), &emb, lexical_sig(text));
        assert_eq!(
            out,
            SubstepLookup::Miss,
            "a mutating sub-step lookup must never be served from cache (R3)"
        );
    }

    /// A hit on a different model OR a different embedding space is refused (R3
    /// — never serve across model/embedding space), even when the vectors are
    /// identical.
    #[test]
    fn substep_cache_never_serves_across_model_or_embedding_space() {
        let cache = SubstepCache::new();
        let emb = vec![1.0_f32, 0.0, 0.0, 0.0];
        let text = "[tool] {\"temp_c\": 12}";
        cache.insert(
            SubstepKind::ReadOnly,
            read_only_entry(emb.clone(), text, 0.02),
        );

        // Same space → hit (the positive control: the gates otherwise pass).
        let hit = cache.lookup(SubstepKind::ReadOnly, &space(), &emb, lexical_sig(text));
        assert!(
            matches!(hit, SubstepLookup::Hit { .. }),
            "same model + embedding space with identical vectors must hit"
        );

        // Different completion model → refused.
        let other_model = SubstepSpace {
            model: "claude-sonnet-4-6".into(),
            embedding_model: "text-embedding-3-small".into(),
        };
        assert_eq!(
            cache.lookup(SubstepKind::ReadOnly, &other_model, &emb, lexical_sig(text)),
            SubstepLookup::Miss,
            "a serve across a different completion model must be refused (R3)"
        );

        // Different embedding space → refused.
        let other_embed = SubstepSpace {
            model: "gpt-4o".into(),
            embedding_model: "text-embedding-3-large".into(),
        };
        assert_eq!(
            cache.lookup(SubstepKind::ReadOnly, &other_embed, &emb, lexical_sig(text)),
            SubstepLookup::Miss,
            "a serve across a different embedding space must be refused (R3)"
        );
    }

    /// Caveat C2: the per-request headline books a REALIZED hit's MEASURED delta
    /// only — never the `weighted = cost·(L1+(1−L1)·L2)` expectation value from
    /// `tt_preview::cache_projection`. The expectation stays a projection
    /// figure, not a billed saving.
    #[test]
    fn substep_cache_books_only_realized_hits_not_expectation() {
        let cache = SubstepCache::new();
        // Differentiate two turns by tool-result content using the NEW axis
        // (gap G4) so the embedding text — and thus the lexical sig — keys on
        // the tool result.
        let req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![
                Message::User {
                    content: MessageContent::Text("look up the weather".into()),
                    name: None,
                },
                Message::Tool {
                    content: MessageContent::Text("{\"temp_c\": 12}".into()),
                    tool_call_id: "call_1".into(),
                },
            ],
            ..Default::default()
        };
        let axis_text = l2_tool_context_text(&req).expect("tool-context axis text");
        assert!(
            axis_text.contains("temp_c"),
            "the realized-hit test must key on the tool-result axis (G4): {axis_text}"
        );

        let realized_cost = 0.0200_f64;
        let emb = vec![1.0_f32, 0.0, 0.0, 0.0];
        cache.insert(
            SubstepKind::ReadOnly,
            read_only_entry(emb.clone(), &axis_text, realized_cost),
        );

        let out = cache.lookup(
            SubstepKind::ReadOnly,
            &space(),
            &emb,
            lexical_sig(&axis_text),
        );
        let SubstepLookup::Hit { saved_usd, .. } = out else {
            panic!("a same-space, above-threshold, lexically-agreeing hit must be a Hit: {out:?}");
        };

        // The booked saving is the MEASURED realized cost — exactly.
        assert!(
            (saved_usd - realized_cost).abs() < 1e-12,
            "a realized hit must book the MEASURED delta ({realized_cost}); got {saved_usd}"
        );

        // It must NOT be the expectation value. With the projection's default
        // hit probabilities, the weighted expectation is strictly less than the
        // realized cost, so a (forbidden) expectation-booking would differ.
        let expectation = tt_preview::cache_projection::project(
            realized_cost,
            tt_preview::cache_projection::DEFAULT_L1_HIT_PROBABILITY,
            tt_preview::cache_projection::DEFAULT_L2_HIT_PROBABILITY,
        )
        .weighted_savings_usd;
        assert!(
            expectation < realized_cost,
            "precondition: the expectation must differ from the realized cost"
        );
        assert!(
            (saved_usd - expectation).abs() > 1e-9,
            "caveat C2: a realized hit must NOT book the {expectation} expectation value"
        );
    }
}
