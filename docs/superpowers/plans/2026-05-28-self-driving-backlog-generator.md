# Self-driving Autopilot Backlog Generator — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a `tt backlog generate` CLI subcommand + GHA workflow that refills the autopilot backlog daily from Tier-1 deterministic signals (Sentry errors, tt-api drift/anomaly/latency signals, inspect-self findings, stale `[BLOCKED]` items). Tier 2 (gh triage) and Tier 3 (LLM proposals) are out of scope here and ship in follow-up plans.

**Architecture:** New module tree at `crates/cli/src/backlog/` with one file per source + shared types, dedupe via GitHub issue body fingerprint search, rate-limited issue creation through a `GhClient` trait (real impl shells to `gh`; tests inject mock). Orchestrator runs sources in parallel, deduplicates, then emits via gh. Audit + cost-ledger writes go through tt-api admin endpoints. GHA workflow runs daily at 14:00 UTC.

**Tech Stack:** Rust 1.88, `clap` for CLI, `reqwest` for HTTP, `serde`/`serde_json`, `sha2` for fingerprinting, `tokio` async runtime, `thiserror` errors, `tracing` logging, `httpmock` + `insta` for tests.

**Preconditions (NOT part of this plan):**
1. Cloud repo `tokentrimmer-cloud` must implement `GET /v1/admin/backlog/signals`, `GET /v1/admin/backlog/telemetry-summary`, `POST /v1/admin/backlog/audit-run` before GHA flips to production. This plan unit-tests with `httpmock`; deployment is gated on cloud-side work.
2. GHA secrets must be added: `SENTRY_AUTH_TOKEN`, `TT_API_BACKLOG_TOKEN`, `TT_API_BASE_URL`. Configure after the plan ships before flipping cron live.

**Scope cuts (deferred):**
- Tier 2 (`gh-triage`) and Tier 3 (`llm-propose`) sources → follow-up plans `02-trackF-tier2-gh-triage.md` and `03-trackF-tier3-llm-propose.md`.
- L2 embedding-similarity secondary dedupe → only matters when Tier 2/3 ships (paraphrase risk).
- Resend inbound webhook (`feedback@`) → Day-60 decision per spec §9.
- `backlog-curator` subagent → spec'd here as a placeholder file for Tier 3 plan to flesh out.

---

## File Structure

```
crates/cli/
├── Cargo.toml                              [modified — add 4 deps]
└── src/
    ├── main.rs                             [modified — add Backlog subcommand]
    └── backlog/
        ├── mod.rs                          [NEW — orchestrator]
        ├── types.rs                        [NEW — BacklogCandidate, Source, Tier, Severity]
        ├── issue.rs                        [NEW — body template, title, fingerprint]
        ├── gh_client.rs                    [NEW — GhClient trait + ProcessGhClient + MockGhClient]
        ├── sentry.rs                       [NEW — Sentry HTTP source]
        ├── signals.rs                      [NEW — tt-api /signals HTTP source]
        ├── inspect.rs                      [NEW — runs `tt inspect` on cwd]
        ├── stale_blocked.rs                [NEW — scans BACKLOG.md for stale BLOCKED]
        ├── dedupe.rs                       [NEW — gh search by fingerprint]
        ├── rate_limit.rs                   [NEW — per-tier-per-source caps]
        ├── audit.rs                        [NEW — POST /audit-run client]
        └── tests/
            ├── orchestrator_smoke.rs       [NEW — end-to-end with httpmock + MockGhClient]
            └── fixtures/                   [NEW — JSON fixtures per source]

.github/workflows/
└── backlog-generate.yml                    [NEW — cron 14:00 UTC]

.claude/
├── agents/backlog-curator.md               [NEW — Sonnet stub for Tier 3 plan]
└── budget.toml                             [modified — add [backlog_generator] block]

scripts/
├── rotate-backlog-token.sh                 [NEW — token rotation helper]
└── backlog-rollback.sh                     [NEW — close issues from last N days]
```

**Responsibility per file:**
- `types.rs` — data shapes only. No I/O.
- `issue.rs` — pure functions: title format, body template, fingerprint hash. No I/O.
- `gh_client.rs` — abstracts `gh issue create` + `gh issue list --search`. Testable.
- `sentry.rs` / `signals.rs` / `inspect.rs` / `stale_blocked.rs` — one source each. Each emits `Vec<BacklogCandidate>`.
- `dedupe.rs` — given candidates + `GhClient`, filters out ones whose fingerprint is already in gh.
- `rate_limit.rs` — applies per-tier-per-source caps to candidate vec.
- `audit.rs` — POSTs the run summary to tt-api.
- `mod.rs` — orchestrates: parallel-fetch sources, dedupe, rate-limit, emit, audit.

---

## Task 1: Add dependencies and scaffold the module tree

**Files:**
- Modify: `crates/cli/Cargo.toml`
- Create: `crates/cli/src/backlog/mod.rs`
- Create: `crates/cli/src/backlog/types.rs`
- Create: `crates/cli/src/backlog/issue.rs`
- Create: `crates/cli/src/backlog/gh_client.rs`
- Create: `crates/cli/src/backlog/sentry.rs`
- Create: `crates/cli/src/backlog/signals.rs`
- Create: `crates/cli/src/backlog/inspect.rs`
- Create: `crates/cli/src/backlog/stale_blocked.rs`
- Create: `crates/cli/src/backlog/dedupe.rs`
- Create: `crates/cli/src/backlog/rate_limit.rs`
- Create: `crates/cli/src/backlog/audit.rs`

- [ ] **Step 1: Add deps to `crates/cli/Cargo.toml`**

In `[dependencies]` block, after the existing `sentry.workspace = true` line:

```toml
reqwest = { workspace = true, features = ["json", "rustls-tls"] }
sha2 = "0.10"
thiserror = { workspace = true }
chrono = { version = "0.4", features = ["serde"] }
```

Below the `[dependencies]` block, add:

```toml
[dev-dependencies]
httpmock = "0.7"
insta = { version = "1.39", features = ["json"] }
tempfile = "3.10"
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 2: Create empty module files**

Run:
```bash
mkdir -p crates/cli/src/backlog/tests/fixtures
for f in mod types issue gh_client sentry signals inspect stale_blocked dedupe rate_limit audit; do
  echo "//! tt backlog generate — \`$f\` module (scaffold; see plan)" > "crates/cli/src/backlog/$f.rs"
done
```

- [ ] **Step 3: Replace `mod.rs` with the public module declarations**

```rust
//! `tt backlog generate` — refill the autopilot backlog from deterministic signals.
//!
//! See `docs/superpowers/specs/2026-05-28-self-driving-backlog-design.md`.

pub mod audit;
pub mod dedupe;
pub mod gh_client;
pub mod inspect;
pub mod issue;
pub mod rate_limit;
pub mod sentry;
pub mod signals;
pub mod stale_blocked;
pub mod types;
```

- [ ] **Step 4: Compile check**

Run: `cargo check -p tt-cli`
Expected: success with several `unused module` warnings (acceptable — modules are empty stubs).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/Cargo.toml crates/cli/src/backlog/
git commit -m "feat(cli): scaffold backlog generator module tree

Track F day-0 MVP. Empty modules to be filled by subsequent tasks.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Define shared types

**Files:**
- Modify: `crates/cli/src/backlog/types.rs`
- Test: inline `#[cfg(test)] mod tests`

- [ ] **Step 1: Write failing tests**

Replace `crates/cli/src/backlog/types.rs`:

```rust
//! Shared data types for the backlog generator.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    One,
    Two,
    Three,
}

impl Tier {
    pub fn as_num(self) -> u8 {
        match self {
            Tier::One => 1,
            Tier::Two => 2,
            Tier::Three => 3,
        }
    }

    /// Default issue label for this tier.
    pub fn default_label(self) -> &'static str {
        match self {
            Tier::One => "autopilot",
            Tier::Two => "autopilot-triaged",
            Tier::Three => "autopilot-proposed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    Sentry,
    #[serde(rename = "signals.drift")]
    SignalsDrift,
    #[serde(rename = "signals.anomaly")]
    SignalsAnomaly,
    #[serde(rename = "signals.latency")]
    SignalsLatency,
    InspectSelf,
    #[serde(rename = "backlog.sh-audit")]
    BacklogShAudit,
    GhTriage,
    LlmPropose,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Sentry => "sentry",
            Source::SignalsDrift => "signals.drift",
            Source::SignalsAnomaly => "signals.anomaly",
            Source::SignalsLatency => "signals.latency",
            Source::InspectSelf => "inspect-self",
            Source::BacklogShAudit => "backlog.sh-audit",
            Source::GhTriage => "gh-triage",
            Source::LlmPropose => "llm-propose",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Subagent {
    RustCrateBuilder,
    ProviderAdapterAuthor,
    InspectRuleAuthor,
    AstroPageBuilder,
    PlanReplayValidator,
    BacklogCurator,
}

impl Subagent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Subagent::RustCrateBuilder => "rust-crate-builder",
            Subagent::ProviderAdapterAuthor => "provider-adapter-author",
            Subagent::InspectRuleAuthor => "inspect-rule-author",
            Subagent::AstroPageBuilder => "astro-page-builder",
            Subagent::PlanReplayValidator => "plan-replay-validator",
            Subagent::BacklogCurator => "backlog-curator",
        }
    }
}

/// One candidate item produced by a source, before dedupe + rate-limit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacklogCandidate {
    pub source: Source,
    pub tier: Tier,
    /// Confidence ∈ \[0, 1\]. Always 1.0 for Tier 1+2 deterministic sources.
    pub confidence: f32,
    pub suggested_subagent: Subagent,
    pub est_cost_usd: f64,
    /// Short summary used in the issue title.
    pub title_summary: String,
    /// Tier-prefix shown in the title before the colon, e.g. "defect", "drift",
    /// "regression", "proposal".
    pub title_tier_prefix: String,
    /// 1–3 sentence "why this matters".
    pub why: String,
    /// Bulleted evidence items (markdown).
    pub evidence: Vec<String>,
    /// 2–4 bullet sketch of the suggested approach.
    pub suggested_approach: Vec<String>,
    /// Specific testable bullets.
    pub acceptance_criteria: Vec<String>,
    /// Stable per-source signal id used in the fingerprint, e.g.
    /// "sentry:abc123" or "drift:org_xyz:2026-05-27".
    pub signal_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_labels_match_spec() {
        assert_eq!(Tier::One.default_label(), "autopilot");
        assert_eq!(Tier::Two.default_label(), "autopilot-triaged");
        assert_eq!(Tier::Three.default_label(), "autopilot-proposed");
    }

    #[test]
    fn tier_numeric_value() {
        assert_eq!(Tier::One.as_num(), 1);
        assert_eq!(Tier::Two.as_num(), 2);
        assert_eq!(Tier::Three.as_num(), 3);
    }

    #[test]
    fn source_str_matches_spec_table() {
        assert_eq!(Source::SignalsDrift.as_str(), "signals.drift");
        assert_eq!(Source::InspectSelf.as_str(), "inspect-self");
        assert_eq!(Source::BacklogShAudit.as_str(), "backlog.sh-audit");
    }

    #[test]
    fn candidate_serde_roundtrip() {
        let c = BacklogCandidate {
            source: Source::Sentry,
            tier: Tier::One,
            confidence: 1.0,
            suggested_subagent: Subagent::RustCrateBuilder,
            est_cost_usd: 0.20,
            title_summary: "x".into(),
            title_tier_prefix: "defect".into(),
            why: "y".into(),
            evidence: vec!["e".into()],
            suggested_approach: vec!["a".into()],
            acceptance_criteria: vec!["ac".into()],
            signal_id: "sentry:1".into(),
        };
        let s = serde_json::to_string(&c).unwrap();
        let c2: BacklogCandidate = serde_json::from_str(&s).unwrap();
        assert_eq!(c2.signal_id, "sentry:1");
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p tt-cli backlog::types`
Expected: 4 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/backlog/types.rs
git commit -m "feat(cli): backlog generator shared types

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Issue template + title + fingerprint (pure functions)

**Files:**
- Modify: `crates/cli/src/backlog/issue.rs`
- Test: inline

- [ ] **Step 1: Write the module + tests**

```rust
//! Pure functions: issue title, body template, fingerprint hash.

use sha2::{Digest, Sha256};

use super::types::BacklogCandidate;

/// `[autopilot] {tier-prefix}: {summary}`
pub fn title(c: &BacklogCandidate) -> String {
    format!("[autopilot] {}: {}", c.title_tier_prefix, c.title_summary)
}

/// SHA-256 of `"{source}:{signal_id}"` — stable across reruns; the dedupe primitive.
pub fn fingerprint(c: &BacklogCandidate) -> String {
    let mut h = Sha256::new();
    h.update(c.source.as_str().as_bytes());
    h.update(b":");
    h.update(c.signal_id.as_bytes());
    hex::encode(h.finalize())
}

pub fn body(c: &BacklogCandidate, generated_at: &str, run_id: &str) -> String {
    let evidence = if c.evidence.is_empty() {
        "_no evidence provided_".to_string()
    } else {
        c.evidence.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n")
    };
    let approach = if c.suggested_approach.is_empty() {
        "_no approach provided_".to_string()
    } else {
        c.suggested_approach.iter().map(|b| format!("- {b}")).collect::<Vec<_>>().join("\n")
    };
    let acceptance = c.acceptance_criteria
        .iter()
        .map(|a| format!("- [ ] {a}"))
        .collect::<Vec<_>>()
        .join("\n");
    let fp = fingerprint(c);

    format!(
        "**Source:** {source}\n\
         **Tier:** {tier}\n\
         **Confidence:** {confidence:.2}\n\
         **Suggested subagent:** {subagent}\n\
         **Est cost:** ${cost:.2}\n\
         **Generated:** {generated_at}\n\
         **Run ID:** {run_id}\n\
         \n\
         ### Why this matters\n{why}\n\
         \n\
         ### Evidence\n{evidence}\n\
         \n\
         ### Suggested approach\n{approach}\n\
         \n\
         ### Acceptance criteria\n{acceptance}\n\
         - [ ] inspect-self clean (no new HIGH/CRITICAL)\n\
         - [ ] cargo test green on touched crate(s)\n\
         \n\
         ---\n\
         <!-- backlog-gen-fingerprint: {fp} -->\n",
        source = c.source.as_str(),
        tier = c.tier.as_num(),
        confidence = c.confidence,
        subagent = c.suggested_subagent.as_str(),
        cost = c.est_cost_usd,
        why = c.why.trim(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backlog::types::{Source, Subagent, Tier};

    fn fixture() -> BacklogCandidate {
        BacklogCandidate {
            source: Source::SignalsDrift,
            tier: Tier::One,
            confidence: 1.0,
            suggested_subagent: Subagent::RustCrateBuilder,
            est_cost_usd: 0.30,
            title_summary: "org_abc reconciliation 4.7% delta on 2026-05-27".into(),
            title_tier_prefix: "drift".into(),
            why: "Drift > 2% breaks the trust-score invariant.".into(),
            evidence: vec!["https://dashboard.tokentrimmer.com/reports/2026-05-27".into()],
            suggested_approach: vec!["Inspect plan_runs.actual_savings vs projected".into()],
            acceptance_criteria: vec!["Drift root-cause documented in ADR".into()],
            signal_id: "drift:org_abc:2026-05-27".into(),
        }
    }

    #[test]
    fn title_format_matches_spec() {
        let c = fixture();
        assert_eq!(
            title(&c),
            "[autopilot] drift: org_abc reconciliation 4.7% delta on 2026-05-27"
        );
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let c = fixture();
        let fp1 = fingerprint(&c);
        let fp2 = fingerprint(&c);
        assert_eq!(fp1, fp2);
        assert_eq!(fp1.len(), 64); // sha256 hex
    }

    #[test]
    fn fingerprint_distinguishes_signal_id() {
        let mut a = fixture();
        let mut b = fixture();
        a.signal_id = "drift:org_abc:2026-05-27".into();
        b.signal_id = "drift:org_xyz:2026-05-27".into();
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn body_contains_fingerprint_comment() {
        let c = fixture();
        let b = body(&c, "2026-05-28T14:00:00Z", "abc1234");
        let fp = fingerprint(&c);
        assert!(b.contains(&format!("<!-- backlog-gen-fingerprint: {fp} -->")));
    }

    #[test]
    fn body_contains_required_headers() {
        let c = fixture();
        let b = body(&c, "2026-05-28T14:00:00Z", "abc1234");
        for required in [
            "**Source:** signals.drift",
            "**Tier:** 1",
            "**Confidence:** 1.00",
            "**Suggested subagent:** rust-crate-builder",
            "**Est cost:** $0.30",
            "**Generated:** 2026-05-28T14:00:00Z",
            "**Run ID:** abc1234",
            "### Why this matters",
            "### Evidence",
            "### Suggested approach",
            "### Acceptance criteria",
            "- [ ] inspect-self clean",
            "- [ ] cargo test green",
        ] {
            assert!(b.contains(required), "missing: {required}\n\nbody:\n{b}");
        }
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p tt-cli backlog::issue`
Expected: 5 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/backlog/issue.rs
git commit -m "feat(cli): backlog issue title, body, fingerprint (pure)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: GhClient trait + ProcessGhClient + MockGhClient

**Files:**
- Modify: `crates/cli/src/backlog/gh_client.rs`
- Test: inline

- [ ] **Step 1: Write the module + tests**

```rust
//! Thin wrapper over the `gh` CLI. Tests inject `MockGhClient`.

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GhError {
    #[error("gh exited with status {status}: {stderr}")]
    NonZeroExit { status: i32, stderr: String },
    #[error("gh process failed: {0}")]
    Process(#[from] std::io::Error),
    #[error("could not parse gh JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("missing GITHUB_REPOSITORY env (set in GHA automatically)")]
    MissingRepoEnv,
}

pub struct CreateIssue<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub label: &'a str,
}

#[async_trait]
pub trait GhClient: Send + Sync {
    /// Returns the URL of the newly-created issue.
    async fn create_issue(&self, req: CreateIssue<'_>) -> Result<String, GhError>;

    /// Returns the issue numbers whose body contains the given fingerprint
    /// across all states (open, closed).
    async fn search_by_fingerprint(&self, fingerprint: &str) -> Result<Vec<u64>, GhError>;

    /// Returns the count of open issues with the given label.
    async fn open_issue_count_with_label(&self, label: &str) -> Result<u64, GhError>;
}

/// Production impl: shells out to `gh`.
pub struct ProcessGhClient;

#[async_trait]
impl GhClient for ProcessGhClient {
    async fn create_issue(&self, req: CreateIssue<'_>) -> Result<String, GhError> {
        let out = tokio::process::Command::new("gh")
            .arg("issue").arg("create")
            .arg("--title").arg(req.title)
            .arg("--body").arg(req.body)
            .arg("--label").arg(req.label)
            .output()
            .await?;
        if !out.status.success() {
            return Err(GhError::NonZeroExit {
                status: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    async fn search_by_fingerprint(&self, fingerprint: &str) -> Result<Vec<u64>, GhError> {
        let search = format!("backlog-gen-fingerprint: {fingerprint} in:body");
        let out = tokio::process::Command::new("gh")
            .arg("issue").arg("list")
            .arg("--state").arg("all")
            .arg("--search").arg(&search)
            .arg("--json").arg("number")
            .arg("--limit").arg("50")
            .output()
            .await?;
        if !out.status.success() {
            return Err(GhError::NonZeroExit {
                status: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout)?;
        Ok(rows.iter().filter_map(|v| v.get("number").and_then(|n| n.as_u64())).collect())
    }

    async fn open_issue_count_with_label(&self, label: &str) -> Result<u64, GhError> {
        let out = tokio::process::Command::new("gh")
            .arg("issue").arg("list")
            .arg("--state").arg("open")
            .arg("--label").arg(label)
            .arg("--json").arg("number")
            .arg("--limit").arg("1000")
            .output()
            .await?;
        if !out.status.success() {
            return Err(GhError::NonZeroExit {
                status: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout)?;
        Ok(rows.len() as u64)
    }
}

/// Test impl: records calls + returns canned responses.
#[derive(Default)]
pub struct MockGhClient {
    pub created: Mutex<Vec<(String, String, String)>>,
    pub existing_fingerprints: Mutex<HashSet<String>>,
    pub open_counts: Mutex<std::collections::HashMap<String, u64>>,
}

impl MockGhClient {
    pub fn with_existing_fingerprint(self, fp: impl Into<String>) -> Self {
        self.existing_fingerprints.lock().unwrap().insert(fp.into());
        self
    }
    pub fn with_open_count(self, label: impl Into<String>, count: u64) -> Self {
        self.open_counts.lock().unwrap().insert(label.into(), count);
        self
    }
    pub fn created_titles(&self) -> Vec<String> {
        self.created.lock().unwrap().iter().map(|(t, _, _)| t.clone()).collect()
    }
}

#[async_trait]
impl GhClient for MockGhClient {
    async fn create_issue(&self, req: CreateIssue<'_>) -> Result<String, GhError> {
        self.created.lock().unwrap().push((
            req.title.to_string(),
            req.body.to_string(),
            req.label.to_string(),
        ));
        Ok(format!("https://github.com/test/repo/issues/{}", self.created.lock().unwrap().len()))
    }
    async fn search_by_fingerprint(&self, fingerprint: &str) -> Result<Vec<u64>, GhError> {
        if self.existing_fingerprints.lock().unwrap().contains(fingerprint) {
            Ok(vec![1])
        } else {
            Ok(vec![])
        }
    }
    async fn open_issue_count_with_label(&self, label: &str) -> Result<u64, GhError> {
        Ok(*self.open_counts.lock().unwrap().get(label).unwrap_or(&0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_records_create_call() {
        let m = MockGhClient::default();
        let url = m.create_issue(CreateIssue {
            title: "[autopilot] defect: x",
            body: "body",
            label: "autopilot",
        }).await.unwrap();
        assert!(url.ends_with("/issues/1"));
        assert_eq!(m.created_titles(), vec!["[autopilot] defect: x"]);
    }

    #[tokio::test]
    async fn mock_search_returns_hit_when_fingerprint_seeded() {
        let m = MockGhClient::default().with_existing_fingerprint("abc");
        assert_eq!(m.search_by_fingerprint("abc").await.unwrap(), vec![1]);
        assert_eq!(m.search_by_fingerprint("xyz").await.unwrap(), Vec::<u64>::new());
    }

    #[tokio::test]
    async fn mock_open_count_returns_seeded_value() {
        let m = MockGhClient::default().with_open_count("autopilot", 7);
        assert_eq!(m.open_issue_count_with_label("autopilot").await.unwrap(), 7);
        assert_eq!(m.open_issue_count_with_label("nope").await.unwrap(), 0);
    }
}
```

- [ ] **Step 2: Add `async-trait` dep to Cargo.toml**

In `crates/cli/Cargo.toml` `[dependencies]`:
```toml
async-trait = "0.1"
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p tt-cli backlog::gh_client`
Expected: 3 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/cli/Cargo.toml crates/cli/src/backlog/gh_client.rs
git commit -m "feat(cli): GhClient trait + Process/Mock impls

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Sentry source

**Files:**
- Modify: `crates/cli/src/backlog/sentry.rs`
- Test: inline with `httpmock`

- [ ] **Step 1: Write the module + tests**

```rust
//! Sentry HTTP source — produces Tier-1 defect candidates.

use serde::Deserialize;
use thiserror::Error;

use super::types::{BacklogCandidate, Source, Subagent, Tier};

#[derive(Debug, Error)]
pub enum SentryError {
    #[error("sentry HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("missing SENTRY_ORG or SENTRY_PROJECT env var")]
    MissingConfig,
}

pub struct SentryConfig {
    pub base_url: String,
    pub auth_token: String,
    pub organization: String,
    pub project: String,
    /// Issues with `count >= min_count` over the lookback window qualify.
    pub min_count: u32,
}

impl SentryConfig {
    pub fn from_env() -> Result<Self, SentryError> {
        let organization = std::env::var("SENTRY_ORG").map_err(|_| SentryError::MissingConfig)?;
        let project = std::env::var("SENTRY_PROJECT").map_err(|_| SentryError::MissingConfig)?;
        let auth_token = std::env::var("SENTRY_AUTH_TOKEN").map_err(|_| SentryError::MissingConfig)?;
        let base_url = std::env::var("SENTRY_BASE_URL").unwrap_or_else(|_| "https://sentry.io".into());
        Ok(Self { base_url, auth_token, organization, project, min_count: 3 })
    }
}

#[derive(Debug, Deserialize)]
struct SentryIssue {
    id: String,
    title: String,
    #[serde(rename = "shortId")]
    short_id: String,
    count: String,
    permalink: String,
}

pub async fn fetch(cfg: &SentryConfig) -> Result<Vec<BacklogCandidate>, SentryError> {
    let url = format!(
        "{}/api/0/projects/{}/{}/issues/?query=is%3Aunresolved+level%3Aerror&statsPeriod=24h",
        cfg.base_url, cfg.organization, cfg.project
    );
    let client = reqwest::Client::new();
    let issues: Vec<SentryIssue> = client
        .get(&url)
        .bearer_auth(&cfg.auth_token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(issues
        .into_iter()
        .filter(|i| i.count.parse::<u32>().unwrap_or(0) >= cfg.min_count)
        .map(to_candidate)
        .collect())
}

fn to_candidate(issue: SentryIssue) -> BacklogCandidate {
    let count = issue.count.clone();
    BacklogCandidate {
        source: Source::Sentry,
        tier: Tier::One,
        confidence: 1.0,
        suggested_subagent: Subagent::RustCrateBuilder,
        est_cost_usd: 0.30,
        title_tier_prefix: "defect".into(),
        title_summary: format!("Sentry {} recurring {}x/24h", issue.short_id, count),
        why: format!(
            "Sentry issue {} ({}) is unresolved with {} occurrences in the last 24h. \
             Recurring errors at this rate harm reliability SLOs.",
            issue.short_id, issue.title, count,
        ),
        evidence: vec![issue.permalink.clone()],
        suggested_approach: vec![
            "Open the Sentry permalink and read the stack trace + breadcrumbs.".into(),
            "Identify which crate owns the failing code path.".into(),
            "Reproduce locally if possible; add a regression test before fixing.".into(),
        ],
        acceptance_criteria: vec![
            format!("Sentry issue {} resolved or noise-filtered with rationale", issue.short_id),
            "Regression test added if root cause is in our code".into(),
        ],
        signal_id: format!("sentry:{}", issue.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn make_cfg(server: &MockServer) -> SentryConfig {
        SentryConfig {
            base_url: server.base_url(),
            auth_token: "token".into(),
            organization: "tokentrimmer".into(),
            project: "tokentrimmer".into(),
            min_count: 3,
        }
    }

    #[tokio::test]
    async fn fetches_and_maps_issues_above_threshold() {
        let server = MockServer::start_async().await;
        let _m = server.mock_async(|when, then| {
            when.method(GET)
                .path("/api/0/projects/tokentrimmer/tokentrimmer/issues/")
                .header("authorization", "Bearer token");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"[
                    {"id": "1", "title": "x", "shortId": "TT-1", "count": "5", "permalink": "https://sentry.io/issues/1"},
                    {"id": "2", "title": "y", "shortId": "TT-2", "count": "2", "permalink": "https://sentry.io/issues/2"}
                ]"#);
        }).await;

        let cs = fetch(&make_cfg(&server)).await.unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].signal_id, "sentry:1");
        assert_eq!(cs[0].title_tier_prefix, "defect");
        assert!(cs[0].title_summary.contains("TT-1"));
        assert!(cs[0].title_summary.contains("5x/24h"));
    }

    #[tokio::test]
    async fn empty_response_yields_no_candidates() {
        let server = MockServer::start_async().await;
        let _m = server.mock_async(|when, then| {
            when.method(GET).path("/api/0/projects/tokentrimmer/tokentrimmer/issues/");
            then.status(200).header("content-type", "application/json").body("[]");
        }).await;

        let cs = fetch(&make_cfg(&server)).await.unwrap();
        assert!(cs.is_empty());
    }

    #[tokio::test]
    async fn sentry_5xx_returns_err() {
        let server = MockServer::start_async().await;
        let _m = server.mock_async(|when, then| {
            when.method(GET).path("/api/0/projects/tokentrimmer/tokentrimmer/issues/");
            then.status(500).body("oops");
        }).await;

        let err = fetch(&make_cfg(&server)).await.unwrap_err();
        assert!(matches!(err, SentryError::Http(_)));
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p tt-cli backlog::sentry`
Expected: 3 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/backlog/sentry.rs
git commit -m "feat(cli): Sentry source (Tier 1)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: tt-api signals source

**Files:**
- Modify: `crates/cli/src/backlog/signals.rs`
- Test: inline with `httpmock`

- [ ] **Step 1: Write the module + tests**

```rust
//! tt-api `/v1/admin/backlog/signals` source — produces drift, anomaly, and
//! latency-regression candidates.

use serde::Deserialize;
use thiserror::Error;

use super::types::{BacklogCandidate, Source, Subagent, Tier};

#[derive(Debug, Error)]
pub enum SignalsError {
    #[error("signals HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("missing TT_API_BASE_URL or TT_API_BACKLOG_TOKEN env var")]
    MissingConfig,
}

pub struct SignalsConfig {
    pub base_url: String,
    pub token: String,
}

impl SignalsConfig {
    pub fn from_env() -> Result<Self, SignalsError> {
        let base_url = std::env::var("TT_API_BASE_URL").map_err(|_| SignalsError::MissingConfig)?;
        let token = std::env::var("TT_API_BACKLOG_TOKEN").map_err(|_| SignalsError::MissingConfig)?;
        Ok(Self { base_url, token })
    }
}

/// Wire shape returned by tt-api. Mirrored field names in cloud crate.
#[derive(Debug, Deserialize)]
pub struct SignalsResponse {
    #[serde(default)]
    pub drift: Vec<DriftSignal>,
    #[serde(default)]
    pub anomaly: Vec<AnomalySignal>,
    #[serde(default)]
    pub latency: Vec<LatencySignal>,
}

#[derive(Debug, Deserialize)]
pub struct DriftSignal {
    pub org_id: String,
    pub plan_run_date: String,
    pub projected_savings_usd: f64,
    pub actual_savings_usd: f64,
    pub delta_pct: f64,
}

#[derive(Debug, Deserialize)]
pub struct AnomalySignal {
    pub org_id: String,
    pub hour_bucket: String,
    pub z_score: f64,
}

#[derive(Debug, Deserialize)]
pub struct LatencySignal {
    pub percentile: String,
    pub cache_layer: String,
    pub date: String,
    pub observed_ms: f64,
    pub threshold_ms: f64,
}

pub async fn fetch(cfg: &SignalsConfig) -> Result<Vec<BacklogCandidate>, SignalsError> {
    let url = format!("{}/v1/admin/backlog/signals", cfg.base_url);
    let resp: SignalsResponse = reqwest::Client::new()
        .get(&url)
        .bearer_auth(&cfg.token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut out = Vec::new();
    out.extend(resp.drift.into_iter().map(drift_to_candidate));
    out.extend(resp.anomaly.into_iter().map(anomaly_to_candidate));
    out.extend(resp.latency.into_iter().map(latency_to_candidate));
    Ok(out)
}

fn drift_to_candidate(d: DriftSignal) -> BacklogCandidate {
    BacklogCandidate {
        source: Source::SignalsDrift,
        tier: Tier::One,
        confidence: 1.0,
        suggested_subagent: Subagent::PlanReplayValidator,
        est_cost_usd: 0.40,
        title_tier_prefix: "drift".into(),
        title_summary: format!(
            "org {} reconciliation {:.1}% delta on {}",
            d.org_id, d.delta_pct * 100.0, d.plan_run_date
        ),
        why: format!(
            "Reconciliation drift of {:.2}% between projected (${:.2}) and actual (${:.2}) \
             savings violates the < 2% honesty invariant.",
            d.delta_pct * 100.0, d.projected_savings_usd, d.actual_savings_usd,
        ),
        evidence: vec![format!(
            "https://dashboard.tokentrimmer.com/reports/{}", d.plan_run_date
        )],
        suggested_approach: vec![
            "Inspect plan_runs.actual_* columns vs projected for the date.".into(),
            "Identify which routes contributed most to the delta.".into(),
            "Adjust the Plan replay assumption that drifted or document why drift is benign.".into(),
        ],
        acceptance_criteria: vec![
            "Root cause identified (replay bug / config change / unforeseen traffic).".into(),
            "Either a fix lands or an ADR documents the acceptable drift.".into(),
        ],
        signal_id: format!("drift:{}:{}", d.org_id, d.plan_run_date),
    }
}

fn anomaly_to_candidate(a: AnomalySignal) -> BacklogCandidate {
    BacklogCandidate {
        source: Source::SignalsAnomaly,
        tier: Tier::One,
        confidence: 1.0,
        suggested_subagent: Subagent::RustCrateBuilder,
        est_cost_usd: 0.30,
        title_tier_prefix: "anomaly".into(),
        title_summary: format!(
            "org {} spend {:.1}σ above expected on {}",
            a.org_id, a.z_score, a.hour_bucket
        ),
        why: format!(
            "Hourly spend exceeded the org's 3σ band. Likely culprits: prompt regression, \
             cache miss spike, or routing rule disabled."
        ),
        evidence: vec![format!(
            "https://dashboard.tokentrimmer.com/costs?org={}&window=24h", a.org_id
        )],
        suggested_approach: vec![
            "Pull the top 10 expensive requests during the spike window.".into(),
            "Diff against the prior week's median to isolate the regression.".into(),
        ],
        acceptance_criteria: vec![
            "Anomaly explained in a comment or labeled benign.".into(),
        ],
        signal_id: format!("anomaly:{}:{}", a.org_id, a.hour_bucket),
    }
}

fn latency_to_candidate(l: LatencySignal) -> BacklogCandidate {
    BacklogCandidate {
        source: Source::SignalsLatency,
        tier: Tier::One,
        confidence: 1.0,
        suggested_subagent: Subagent::RustCrateBuilder,
        est_cost_usd: 0.30,
        title_tier_prefix: "regression".into(),
        title_summary: format!(
            "{} {} {:.1}ms > {:.1}ms on {}",
            l.percentile, l.cache_layer, l.observed_ms, l.threshold_ms, l.date
        ),
        why: format!(
            "Gateway latency SLO breached: {} on {} cache reached {:.1}ms (threshold {:.1}ms).",
            l.percentile, l.cache_layer, l.observed_ms, l.threshold_ms,
        ),
        evidence: vec!["latency-smoke logs in CI artifacts".into()],
        suggested_approach: vec![
            "Run `oha` against local Gateway with the same workload.".into(),
            "Compare flamegraphs against the last known-good revision.".into(),
        ],
        acceptance_criteria: vec![
            format!("{} {} cache restored to ≤ {:.1}ms in latency-smoke", l.percentile, l.cache_layer, l.threshold_ms),
        ],
        signal_id: format!("latency:{}:{}:{}", l.percentile, l.cache_layer, l.date),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn maps_all_three_signal_kinds() {
        let server = MockServer::start_async().await;
        let _m = server.mock_async(|when, then| {
            when.method(GET).path("/v1/admin/backlog/signals");
            then.status(200).header("content-type", "application/json").body(r#"{
                "drift": [{"org_id": "org_abc", "plan_run_date": "2026-05-27", "projected_savings_usd": 100.0, "actual_savings_usd": 95.0, "delta_pct": 0.05}],
                "anomaly": [{"org_id": "org_xyz", "hour_bucket": "2026-05-27T14:00:00Z", "z_score": 4.2}],
                "latency": [{"percentile": "p50", "cache_layer": "miss", "date": "2026-05-27", "observed_ms": 42.0, "threshold_ms": 30.0}]
            }"#);
        }).await;

        let cfg = SignalsConfig { base_url: server.base_url(), token: "t".into() };
        let cs = fetch(&cfg).await.unwrap();
        assert_eq!(cs.len(), 3);
        assert_eq!(cs[0].signal_id, "drift:org_abc:2026-05-27");
        assert_eq!(cs[1].signal_id, "anomaly:org_xyz:2026-05-27T14:00:00Z");
        assert_eq!(cs[2].signal_id, "latency:p50:miss:2026-05-27");
    }

    #[tokio::test]
    async fn empty_response_is_ok() {
        let server = MockServer::start_async().await;
        let _m = server.mock_async(|when, then| {
            when.method(GET).path("/v1/admin/backlog/signals");
            then.status(200).body("{}");
        }).await;
        let cfg = SignalsConfig { base_url: server.base_url(), token: "t".into() };
        assert!(fetch(&cfg).await.unwrap().is_empty());
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p tt-cli backlog::signals`
Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/backlog/signals.rs
git commit -m "feat(cli): tt-api signals source (Tier 1)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Inspect-self source

**Files:**
- Modify: `crates/cli/src/backlog/inspect.rs`
- Test: inline using `tt-inspect-core` directly (no subprocess)

- [ ] **Step 1: Write the module + tests**

```rust
//! Inspect-self source — runs the existing `tt-inspect-core` engine over the
//! current working directory and emits one Tier-1 candidate per new
//! HIGH/CRITICAL finding.

use thiserror::Error;
use tt_inspect_core::{Severity, Finding};

use super::types::{BacklogCandidate, Source, Subagent, Tier};

#[derive(Debug, Error)]
pub enum InspectError {
    #[error("inspect engine: {0}")]
    Engine(String),
}

pub fn fetch(path: &std::path::Path) -> Result<Vec<BacklogCandidate>, InspectError> {
    // Delegate to the same engine the `tt inspect` subcommand uses. The
    // `scan_path` entry point lives in tt-inspect-core; if its signature
    // differs in your tree, see crates/cli/src/main.rs Inspect handler for the
    // current call site to mimic.
    let findings = tt_inspect_core::scan_path(path)
        .map_err(|e| InspectError::Engine(e.to_string()))?;
    Ok(findings
        .into_iter()
        .filter(|f| matches!(f.severity, Severity::High | Severity::Critical))
        .map(to_candidate)
        .collect())
}

fn to_candidate(f: Finding) -> BacklogCandidate {
    BacklogCandidate {
        source: Source::InspectSelf,
        tier: Tier::One,
        confidence: 1.0,
        suggested_subagent: Subagent::RustCrateBuilder,
        est_cost_usd: 0.20,
        title_tier_prefix: "inspect".into(),
        title_summary: format!("{} at {}:{}", f.rule_id, f.file.display(), f.line),
        why: format!(
            "Dogfood gate: `tt inspect` flagged {} at {}:{} ({}). Letting this land in main \
             violates the inspect-self CI gate.",
            f.rule_id, f.file.display(), f.line, f.severity_str(),
        ),
        evidence: vec![format!("{}:{}", f.file.display(), f.line)],
        suggested_approach: vec![
            format!("Read the rule definition for `{}` in crates/inspect-rules-tier1/.", f.rule_id),
            "Apply the fix at the cited file:line.".into(),
            "Re-run `./scripts/tt-inspect-self.sh` to confirm clean.".into(),
        ],
        acceptance_criteria: vec![
            format!("`{}` no longer fires on {}", f.rule_id, f.file.display()),
            "`./scripts/tt-inspect-self.sh` exits 0".into(),
        ],
        signal_id: format!("inspect:{}:{}:{}", f.rule_id, f.file.display(), f.line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_finding(rule: &str, file: &str, line: usize, sev: Severity) -> Finding {
        Finding {
            rule_id: rule.into(),
            file: PathBuf::from(file),
            line,
            severity: sev,
            message: "x".into(),
        }
    }

    #[test]
    fn filters_to_high_and_critical_only() {
        let fs = vec![
            make_finding("r1", "a.rs", 1, Severity::Low),
            make_finding("r2", "b.rs", 2, Severity::High),
            make_finding("r3", "c.rs", 3, Severity::Critical),
        ];
        let cs: Vec<_> = fs.into_iter()
            .filter(|f| matches!(f.severity, Severity::High | Severity::Critical))
            .map(to_candidate).collect();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].signal_id, "inspect:r2:b.rs:2");
        assert_eq!(cs[1].signal_id, "inspect:r3:c.rs:3");
    }

    #[test]
    fn candidate_title_includes_rule_and_location() {
        let f = make_finding("output-no-max-tokens", "src/lib.rs", 42, Severity::High);
        let c = to_candidate(f);
        assert_eq!(c.title_summary, "output-no-max-tokens at src/lib.rs:42");
        assert_eq!(c.title_tier_prefix, "inspect");
    }
}
```

- [ ] **Step 2: Verify `tt_inspect_core::scan_path` signature**

Run: `grep -rn 'pub fn scan_path\|pub fn scan(' crates/inspect-core/src/`
- If the public entry point is named differently (e.g. `Engine::run`, `scan`, etc.), update the call site in `fetch()` to match. The Inspect CLI handler in `crates/cli/src/main.rs` shows the canonical invocation — mirror that. Also confirm `Finding` exposes `rule_id`, `file`, `line`, `severity` publicly; if not, add `pub` to those fields or expose accessors in `tt-inspect-core`.

- [ ] **Step 3: Run tests**

Run: `cargo test -p tt-cli backlog::inspect`
Expected: 2 passed (subprocess integration tested at orchestrator level).

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/backlog/inspect.rs
git commit -m "feat(cli): inspect-self source (Tier 1)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Stale BLOCKED source

**Files:**
- Modify: `crates/cli/src/backlog/stale_blocked.rs`
- Test: inline using `tempfile`

- [ ] **Step 1: Write the module + tests**

```rust
//! Scans BACKLOG.md for items marked `[BLOCKED — ...]` and flags any whose
//! containing section header timestamp is older than 7 days. Emits
//! `autopilot-triaged` candidates so the human notices stalled items.

use std::path::Path;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use thiserror::Error;

use super::types::{BacklogCandidate, Source, Subagent, Tier};

#[derive(Debug, Error)]
pub enum StaleError {
    #[error("read BACKLOG: {0}")]
    Io(#[from] std::io::Error),
}

/// Returns one candidate per BLOCKED item whose containing line was last
/// modified > 7 days ago (file mtime as a coarse proxy; the cron firing
/// daily means the granularity is good enough).
pub fn fetch(backlog_path: &Path, now: SystemTime) -> Result<Vec<BacklogCandidate>, StaleError> {
    let mtime = backlog_path.metadata()?.modified()?;
    let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
    if age < Duration::from_secs(7 * 24 * 60 * 60) {
        return Ok(vec![]);
    }
    let text = std::fs::read_to_string(backlog_path)?;
    let now_dt: DateTime<Utc> = now.into();
    let now_str = now_dt.format("%Y-%m-%d").to_string();
    Ok(text
        .lines()
        .filter_map(parse_blocked_line)
        .map(|(task_id, reason)| to_candidate(task_id, reason, &now_str))
        .collect())
}

fn parse_blocked_line(line: &str) -> Option<(String, String)> {
    // Looks for: "- [ ] [PRIORITY] [task-id] subagent: desc ... [BLOCKED — reason]"
    if !line.contains("[BLOCKED") {
        return None;
    }
    let task_id = extract_task_id(line)?;
    let reason = extract_blocked_reason(line)?;
    Some((task_id, reason))
}

fn extract_task_id(line: &str) -> Option<String> {
    // Second pair of square brackets in the line.
    let mut iter = line.match_indices('[');
    let _priority = iter.next()?;
    let task_open = iter.next()?.0 + 1;
    let close = line[task_open..].find(']')? + task_open;
    Some(line[task_open..close].to_string())
}

fn extract_blocked_reason(line: &str) -> Option<String> {
    let start = line.find("[BLOCKED")?;
    let after = &line[start..];
    let close = after.find(']')?;
    Some(after[1..close].to_string()) // strip leading '['
}

fn to_candidate(task_id: String, reason: String, today: &str) -> BacklogCandidate {
    let why = format!(
        "Backlog item `{task_id}` has been marked {reason} for at least 7 days. \
         Either the blocker can now be cleared or the item should be moved to \
         the `Completed` / `Won't do` section."
    );
    BacklogCandidate {
        source: Source::BacklogShAudit,
        tier: Tier::One, // tier-1 (deterministic) but emits at autopilot-triaged label
        confidence: 1.0,
        suggested_subagent: Subagent::RustCrateBuilder,
        est_cost_usd: 0.05,
        title_tier_prefix: "stalled".into(),
        title_summary: format!("[{task_id}] BLOCKED > 7d — needs human unblock"),
        why,
        evidence: vec![format!(".claude/BACKLOG.md (search for `{task_id}`)")],
        suggested_approach: vec![
            "Review the blocker reason inline in BACKLOG.md.".into(),
            "Either unblock (precondition shipped), retire (won't do), or extend the reason with the new blocker.".into(),
        ],
        acceptance_criteria: vec![
            format!("BACKLOG.md item `{task_id}` is no longer in `[BLOCKED]` state OR it is moved to Completed/Won't do."),
        ],
        signal_id: format!("blocked:{}:{}", task_id, today),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    #[test]
    fn extracts_task_id_from_canonical_line() {
        let line = "- [ ] [P2] [w0-fp-rate-script] write `scripts/measure-fp-rate.sh` for Inspect [BLOCKED — needs Week 14 rules]";
        assert_eq!(extract_task_id(line).as_deref(), Some("w0-fp-rate-script"));
    }

    #[test]
    fn extracts_blocked_reason() {
        let line = "- [ ] [P2] [w0-fp-rate-script] foo [BLOCKED — needs rules]";
        assert_eq!(extract_blocked_reason(line).as_deref(), Some("BLOCKED — needs rules"));
    }

    #[test]
    fn parse_returns_none_for_non_blocked_line() {
        let line = "- [x] [P0] [w1-axum-skeleton] rust-crate-builder: did the thing";
        assert!(parse_blocked_line(line).is_none());
    }

    #[test]
    fn returns_empty_when_file_recently_modified() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "- [ ] [P2] [foo] x [BLOCKED — y]").unwrap();
        // mtime is "now" — < 7d threshold.
        let result = fetch(f.path(), SystemTime::now()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn emits_candidate_per_blocked_line_when_file_is_stale() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "- [ ] [P2] [foo] x [BLOCKED — needs Y]").unwrap();
        writeln!(f, "- [ ] [P3] [bar] z [BLOCKED — needs Z]").unwrap();
        writeln!(f, "- [x] [P0] [baz] did the thing").unwrap();
        // Simulate "now" being 8 days after the file was created.
        let now = SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60);
        let cs = fetch(f.path(), now).unwrap();
        assert_eq!(cs.len(), 2);
        assert!(cs[0].signal_id.starts_with("blocked:foo:"));
        assert!(cs[1].signal_id.starts_with("blocked:bar:"));
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p tt-cli backlog::stale_blocked`
Expected: 5 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/backlog/stale_blocked.rs
git commit -m "feat(cli): stale BLOCKED source (Tier 1)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Dedupe + rate-limit

**Files:**
- Modify: `crates/cli/src/backlog/dedupe.rs`
- Modify: `crates/cli/src/backlog/rate_limit.rs`
- Test: inline

- [ ] **Step 1: Write `dedupe.rs`**

```rust
//! Filters out candidates whose fingerprint already exists as a gh issue.

use super::gh_client::{GhClient, GhError};
use super::issue::fingerprint;
use super::types::BacklogCandidate;

pub async fn dedupe(
    candidates: Vec<BacklogCandidate>,
    gh: &dyn GhClient,
) -> Result<(Vec<BacklogCandidate>, usize), GhError> {
    let mut kept = Vec::with_capacity(candidates.len());
    let mut deduped = 0;
    for c in candidates {
        let fp = fingerprint(&c);
        let existing = gh.search_by_fingerprint(&fp).await?;
        if existing.is_empty() {
            kept.push(c);
        } else {
            deduped += 1;
            tracing::info!(fingerprint=%fp, signal_id=%c.signal_id, "deduped (already exists)");
        }
    }
    Ok((kept, deduped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backlog::gh_client::MockGhClient;
    use crate::backlog::types::{Source, Subagent, Tier};

    fn make_candidate(signal_id: &str) -> BacklogCandidate {
        BacklogCandidate {
            source: Source::Sentry,
            tier: Tier::One,
            confidence: 1.0,
            suggested_subagent: Subagent::RustCrateBuilder,
            est_cost_usd: 0.1,
            title_tier_prefix: "defect".into(),
            title_summary: "x".into(),
            why: "y".into(),
            evidence: vec![],
            suggested_approach: vec![],
            acceptance_criteria: vec![],
            signal_id: signal_id.into(),
        }
    }

    #[tokio::test]
    async fn keeps_only_new_candidates() {
        let a = make_candidate("a");
        let b = make_candidate("b");
        let fp_a = fingerprint(&a);
        let mock = MockGhClient::default().with_existing_fingerprint(fp_a);
        let (kept, deduped) = dedupe(vec![a, b], &mock).await.unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].signal_id, "b");
        assert_eq!(deduped, 1);
    }
}
```

- [ ] **Step 2: Write `rate_limit.rs`**

```rust
//! Apply per-tier-per-source caps to a candidate vec. The orchestrator also
//! consults `GhClient::open_issue_count_with_label` to enforce the
//! "active autopilot-proposed ≤ 10 / active autopilot ≤ 20" global caps.

use std::collections::HashMap;

use super::types::{BacklogCandidate, Source, Tier};

pub struct Limits {
    pub total_per_run: usize,
    pub tier_three_per_run: usize,
    pub tier_two_per_run: usize,
    pub tier_one_per_source_per_run: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            total_per_run: 10,
            tier_three_per_run: 3,
            tier_two_per_run: 5,
            tier_one_per_source_per_run: 5,
        }
    }
}

/// Apply caps in order: per-tier-per-source → per-tier-total → per-run-total.
/// Items keep their order (deterministic). Returns (kept, dropped_count).
pub fn apply(mut candidates: Vec<BacklogCandidate>, limits: &Limits) -> (Vec<BacklogCandidate>, usize) {
    let total_in = candidates.len();
    let mut per_source: HashMap<Source, usize> = HashMap::new();
    let mut tier_two = 0;
    let mut tier_three = 0;
    candidates.retain(|c| {
        match c.tier {
            Tier::One => {
                let n = per_source.entry(c.source.clone()).or_insert(0);
                if *n >= limits.tier_one_per_source_per_run { return false; }
                *n += 1;
                true
            }
            Tier::Two => {
                if tier_two >= limits.tier_two_per_run { return false; }
                tier_two += 1;
                true
            }
            Tier::Three => {
                if tier_three >= limits.tier_three_per_run { return false; }
                tier_three += 1;
                true
            }
        }
    });
    if candidates.len() > limits.total_per_run {
        candidates.truncate(limits.total_per_run);
    }
    let dropped = total_in - candidates.len();
    (candidates, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backlog::types::Subagent;

    fn make(source: Source, tier: Tier, n: u32) -> BacklogCandidate {
        BacklogCandidate {
            source,
            tier,
            confidence: 1.0,
            suggested_subagent: Subagent::RustCrateBuilder,
            est_cost_usd: 0.1,
            title_tier_prefix: "x".into(),
            title_summary: format!("n={n}"),
            why: "y".into(),
            evidence: vec![],
            suggested_approach: vec![],
            acceptance_criteria: vec![],
            signal_id: format!("s:{n}"),
        }
    }

    #[test]
    fn caps_tier1_per_source() {
        let cs: Vec<_> = (0..10).map(|i| make(Source::Sentry, Tier::One, i)).collect();
        let (kept, dropped) = apply(cs, &Limits::default());
        assert_eq!(kept.len(), 5);
        assert_eq!(dropped, 5);
    }

    #[test]
    fn caps_total_per_run() {
        let mut cs = Vec::new();
        for s in [Source::Sentry, Source::SignalsDrift, Source::SignalsAnomaly] {
            for i in 0..5 { cs.push(make(s.clone(), Tier::One, i)); }
        }
        // 15 candidates: 5 per source × 3 sources. Per-source cap allows all 15;
        // total cap of 10 truncates.
        let (kept, dropped) = apply(cs, &Limits::default());
        assert_eq!(kept.len(), 10);
        assert_eq!(dropped, 5);
    }

    #[test]
    fn tier3_capped_at_3() {
        let cs: Vec<_> = (0..6).map(|i| make(Source::LlmPropose, Tier::Three, i)).collect();
        let (kept, _) = apply(cs, &Limits::default());
        assert_eq!(kept.len(), 3);
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p tt-cli backlog::dedupe backlog::rate_limit`
Expected: 4 passed total.

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/backlog/dedupe.rs crates/cli/src/backlog/rate_limit.rs
git commit -m "feat(cli): backlog dedupe + rate-limit

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Audit emit client

**Files:**
- Modify: `crates/cli/src/backlog/audit.rs`
- Test: inline with `httpmock`

- [ ] **Step 1: Write the module + tests**

```rust
//! Audit-run POST client — tells tt-api to append a `backlog.generator.run`
//! audit row.

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("audit-run HTTP: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Serialize)]
pub struct AuditRunSummary {
    pub run_id: String,
    pub opened: u32,
    pub deduped: u32,
    pub cost_usd: f64,
    pub skipped_sources: Vec<String>,
}

pub async fn emit(
    base_url: &str,
    token: &str,
    summary: &AuditRunSummary,
) -> Result<(), AuditError> {
    let url = format!("{base_url}/v1/admin/backlog/audit-run");
    reqwest::Client::new()
        .post(&url)
        .bearer_auth(token)
        .json(summary)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn posts_summary_to_audit_endpoint() {
        let server = MockServer::start_async().await;
        let m = server.mock_async(|when, then| {
            when.method(POST)
                .path("/v1/admin/backlog/audit-run")
                .header("authorization", "Bearer t")
                .body_contains(r#""run_id":"abc""#);
            then.status(204);
        }).await;

        let summary = AuditRunSummary {
            run_id: "abc".into(),
            opened: 4,
            deduped: 2,
            cost_usd: 0.0,
            skipped_sources: vec!["llm-propose".into()],
        };
        emit(&server.base_url(), "t", &summary).await.unwrap();
        m.assert_async().await;
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p tt-cli backlog::audit`
Expected: 1 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/backlog/audit.rs
git commit -m "feat(cli): audit-run POST client

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Orchestrator + CLI subcommand wiring

**Files:**
- Modify: `crates/cli/src/backlog/mod.rs`
- Modify: `crates/cli/src/main.rs`
- Test: inline + integration test in Task 12

- [ ] **Step 1: Replace `mod.rs` with orchestrator + public `run()`**

```rust
//! `tt backlog generate` — refill the autopilot backlog from deterministic signals.
//!
//! See `docs/superpowers/specs/2026-05-28-self-driving-backlog-design.md`.

pub mod audit;
pub mod dedupe;
pub mod gh_client;
pub mod inspect;
pub mod issue;
pub mod rate_limit;
pub mod sentry;
pub mod signals;
pub mod stale_blocked;
pub mod types;

use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use thiserror::Error;

use gh_client::{CreateIssue, GhClient, ProcessGhClient};
use rate_limit::{apply as apply_rate_limit, Limits};
use types::{BacklogCandidate, Tier};

#[derive(Debug, Error)]
pub enum RunError {
    #[error("sentry: {0}")]
    Sentry(#[from] sentry::SentryError),
    #[error("signals: {0}")]
    Signals(#[from] signals::SignalsError),
    #[error("inspect: {0}")]
    Inspect(#[from] inspect::InspectError),
    #[error("stale_blocked: {0}")]
    StaleBlocked(#[from] stale_blocked::StaleError),
    #[error("gh: {0}")]
    Gh(#[from] gh_client::GhError),
    #[error("audit: {0}")]
    Audit(#[from] audit::AuditError),
}

pub struct RunOptions {
    pub repo_root: std::path::PathBuf,
    pub backlog_path: std::path::PathBuf,
    pub run_id: String,
    pub dry_run: bool,
    pub limits: Limits,
}

pub struct RunReport {
    pub opened: u32,
    pub deduped: u32,
    pub rate_limited: u32,
    pub skipped_sources: Vec<String>,
}

/// Top-level orchestrator. Sources are fetched in parallel; failures in any
/// one source are logged and that source contributes zero candidates.
pub async fn run(opts: RunOptions, gh: &dyn GhClient) -> Result<RunReport, RunError> {
    let mut candidates: Vec<BacklogCandidate> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    // Sentry
    match sentry::SentryConfig::from_env() {
        Ok(cfg) => match sentry::fetch(&cfg).await {
            Ok(mut cs) => candidates.append(&mut cs),
            Err(e) => {
                tracing::warn!(error=%e, "sentry source skipped");
                skipped.push("sentry".into());
            }
        },
        Err(e) => {
            tracing::warn!(error=%e, "sentry config missing — skipping");
            skipped.push("sentry".into());
        }
    }

    // tt-api signals
    match signals::SignalsConfig::from_env() {
        Ok(cfg) => match signals::fetch(&cfg).await {
            Ok(mut cs) => candidates.append(&mut cs),
            Err(e) => {
                tracing::warn!(error=%e, "signals source skipped");
                skipped.push("signals".into());
            }
        },
        Err(e) => {
            tracing::warn!(error=%e, "signals config missing — skipping");
            skipped.push("signals".into());
        }
    }

    // inspect-self
    match inspect::fetch(&opts.repo_root) {
        Ok(mut cs) => candidates.append(&mut cs),
        Err(e) => {
            tracing::warn!(error=%e, "inspect-self skipped");
            skipped.push("inspect-self".into());
        }
    }

    // stale BLOCKED
    match stale_blocked::fetch(&opts.backlog_path, SystemTime::now()) {
        Ok(mut cs) => candidates.append(&mut cs),
        Err(e) => {
            tracing::warn!(error=%e, "stale_blocked skipped");
            skipped.push("backlog.sh-audit".into());
        }
    }

    let total_before = candidates.len();
    let (candidates, deduped) = dedupe::dedupe(candidates, gh).await?;
    let (candidates, rate_limited) = apply_rate_limit(candidates, &opts.limits);

    // Check global caps: do not open Tier-1 if approved queue is already deep, etc.
    let open_autopilot = gh.open_issue_count_with_label("autopilot").await?;
    let open_proposed = gh.open_issue_count_with_label("autopilot-proposed").await?;

    let mut opened = 0u32;
    let generated_at = DateTime::<Utc>::from(SystemTime::now()).to_rfc3339();
    for c in &candidates {
        // Per-spec global caps
        let too_many_approved = open_autopilot + (opened as u64) >= 20 && c.tier == Tier::One;
        let too_many_proposed = open_proposed + (opened as u64) >= 10 && c.tier == Tier::Three;
        if too_many_approved || too_many_proposed {
            tracing::info!(signal_id=%c.signal_id, "global cap reached; skipping");
            continue;
        }
        let title = issue::title(c);
        let body = issue::body(c, &generated_at, &opts.run_id);
        let label = c.tier.default_label();
        if opts.dry_run {
            tracing::info!(title=%title, label=%label, "[dry-run] would open issue");
        } else {
            let url = gh.create_issue(CreateIssue { title: &title, body: &body, label }).await?;
            tracing::info!(url=%url, "opened issue");
        }
        opened += 1;
    }

    let _ = total_before; // keep for future telemetry
    Ok(RunReport {
        opened,
        deduped: deduped as u32,
        rate_limited: rate_limited as u32,
        skipped_sources: skipped,
    })
}

/// Convenience for `main.rs` — wires the real `ProcessGhClient`.
pub async fn run_default(opts: RunOptions) -> Result<RunReport, RunError> {
    let gh = ProcessGhClient;
    run(opts, &gh).await
}

/// Returns a short, sortable run id derived from the current system time.
pub fn make_run_id() -> String {
    let now: DateTime<Utc> = SystemTime::now().into();
    now.format("%Y%m%dT%H%M%SZ").to_string()
}

pub use rate_limit::Limits as RateLimits;
```

- [ ] **Step 2: Register the subcommand in `crates/cli/src/main.rs`**

In the `Command` enum, after the `Audit { ... }` variant, add:

```rust
    /// Self-driving backlog generator: scans signals and opens GitHub issues.
    Backlog {
        #[command(subcommand)]
        action: BacklogAction,
    },
```

After `enum AuditAction { ... }`, add:

```rust
#[derive(Subcommand)]
enum BacklogAction {
    /// Run all enabled sources and open one issue per surviving candidate.
    Generate {
        /// Skip writing — log what would be opened.
        #[arg(long)]
        dry_run: bool,
        /// Override BACKLOG.md path (default `.claude/BACKLOG.md`).
        #[arg(long, default_value = ".claude/BACKLOG.md")]
        backlog_path: String,
    },
}
```

In the `main` body (look for the existing `match cli.command` dispatch), add:

```rust
        Command::Backlog { action } => match action {
            BacklogAction::Generate { dry_run, backlog_path } => {
                use tt_cli::backlog::{make_run_id, run_default, RateLimits, RunOptions};
                let repo_root = std::env::current_dir().context("getcwd")?;
                let opts = RunOptions {
                    repo_root,
                    backlog_path: backlog_path.into(),
                    run_id: make_run_id(),
                    dry_run,
                    limits: RateLimits::default(),
                };
                let report = run_default(opts).await?;
                println!(
                    "opened={} deduped={} rate_limited={} skipped={:?}",
                    report.opened, report.deduped, report.rate_limited, report.skipped_sources
                );
            }
        },
```

- [ ] **Step 3: Add the public module declaration to the crate root**

Add to the top of `crates/cli/src/main.rs` (or `src/lib.rs` if one exists):

```rust
pub mod backlog;
```

- [ ] **Step 4: Verify build**

Run: `cargo check -p tt-cli`
Expected: clean. Resolve any path errors by adjusting `pub mod backlog;` location to wherever the crate already exposes modules.

- [ ] **Step 5: Clippy clean**

Run: `cargo clippy -p tt-cli -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/cli/src/backlog/mod.rs crates/cli/src/main.rs
git commit -m "feat(cli): wire \`tt backlog generate\` subcommand + orchestrator

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: End-to-end smoke test

**Files:**
- Create: `crates/cli/tests/backlog_smoke.rs`

- [ ] **Step 1: Write the integration test**

```rust
//! End-to-end: feed mocked Sentry + signals + a temp BACKLOG.md through the
//! orchestrator and verify that the right gh issues get "created" via the mock.

use std::time::Duration;

use httpmock::prelude::*;
use tt_cli::backlog::{
    gh_client::MockGhClient,
    rate_limit::Limits,
    run, RunOptions,
};

#[tokio::test(flavor = "multi_thread")]
async fn orchestrator_creates_one_issue_per_surviving_candidate() {
    // Sentry mock
    let sentry_server = MockServer::start_async().await;
    let _s = sentry_server.mock_async(|when, then| {
        when.method(GET).path("/api/0/projects/tt/tt/issues/");
        then.status(200).header("content-type", "application/json").body(r#"[
            {"id":"1","title":"x","shortId":"TT-1","count":"7","permalink":"https://sentry.io/i/1"}
        ]"#);
    }).await;

    // tt-api signals mock
    let signals_server = MockServer::start_async().await;
    let _g = signals_server.mock_async(|when, then| {
        when.method(GET).path("/v1/admin/backlog/signals");
        then.status(200).body(r#"{
            "drift":[{"org_id":"o","plan_run_date":"2026-05-27","projected_savings_usd":10.0,"actual_savings_usd":9.0,"delta_pct":0.10}],
            "anomaly":[],
            "latency":[]
        }"#);
    }).await;

    // Stale BACKLOG fixture
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), "- [ ] [P2] [foo] x [BLOCKED — needs Y]\n").unwrap();
    filetime::set_file_mtime(
        tmp.path(),
        filetime::FileTime::from_system_time(std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60)),
    ).unwrap();

    // Env injection
    std::env::set_var("SENTRY_AUTH_TOKEN", "t");
    std::env::set_var("SENTRY_ORG", "tt");
    std::env::set_var("SENTRY_PROJECT", "tt");
    std::env::set_var("SENTRY_BASE_URL", sentry_server.base_url());
    std::env::set_var("TT_API_BASE_URL", signals_server.base_url());
    std::env::set_var("TT_API_BACKLOG_TOKEN", "t");

    let gh = MockGhClient::default();
    let opts = RunOptions {
        repo_root: std::env::current_dir().unwrap(),
        backlog_path: tmp.path().to_path_buf(),
        run_id: "test".into(),
        dry_run: false,
        limits: Limits::default(),
    };

    let report = run(opts, &gh).await.unwrap();

    // inspect-self may add candidates depending on repo state — assert AT LEAST
    // our three deterministic ones (sentry, drift, stalled-blocked) opened.
    assert!(report.opened >= 3, "report = {report:?}");
    let titles = gh.created_titles();
    assert!(titles.iter().any(|t| t.contains("[autopilot] defect: Sentry TT-1")), "titles = {titles:?}");
    assert!(titles.iter().any(|t| t.contains("[autopilot] drift:")), "titles = {titles:?}");
    assert!(titles.iter().any(|t| t.contains("[autopilot] stalled:") || t.contains("stalled")), "titles = {titles:?}");
}
```

- [ ] **Step 2: Add `filetime` dev-dep**

In `crates/cli/Cargo.toml` `[dev-dependencies]`:
```toml
filetime = "0.2"
```

- [ ] **Step 3: Run test**

Run: `cargo test -p tt-cli --test backlog_smoke`
Expected: 1 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/cli/Cargo.toml crates/cli/tests/backlog_smoke.rs
git commit -m "test(cli): backlog orchestrator end-to-end smoke

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 13: GHA workflow

**Files:**
- Create: `.github/workflows/backlog-generate.yml`

- [ ] **Step 1: Write the workflow**

```yaml
name: Backlog generate (autopilot)

on:
  schedule:
    # 14:00 UTC daily
    - cron: '0 14 * * *'
  workflow_dispatch:
    inputs:
      dry_run:
        description: "Log issues that would be opened but don't actually open them"
        required: false
        default: 'false'

permissions:
  contents: read
  issues: write

concurrency:
  group: backlog-generate
  cancel-in-progress: false

jobs:
  generate:
    runs-on: ubuntu-latest
    timeout-minutes: 15
    env:
      SENTRY_ORG: ${{ vars.SENTRY_ORG }}
      SENTRY_PROJECT: ${{ vars.SENTRY_PROJECT }}
      SENTRY_AUTH_TOKEN: ${{ secrets.SENTRY_AUTH_TOKEN }}
      TT_API_BASE_URL: ${{ vars.TT_API_BASE_URL }}
      TT_API_BACKLOG_TOKEN: ${{ secrets.TT_API_BACKLOG_TOKEN }}
      GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
      RUST_LOG: info
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          toolchain: "1.88"

      - name: Cache cargo registry
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-backlog-${{ hashFiles('**/Cargo.lock') }}

      - name: Build tt
        run: cargo build -p tt-cli --release

      - name: Run backlog generate
        run: |
          set -euo pipefail
          if [[ "${{ github.event.inputs.dry_run }}" == "true" ]]; then
            ./target/release/tt backlog generate --dry-run
          else
            ./target/release/tt backlog generate
          fi

      - name: Step summary
        if: always()
        run: |
          {
            echo "## Backlog generate run"
            echo ""
            echo "- Trigger: ${{ github.event_name }}"
            echo "- Dry run: ${{ github.event.inputs.dry_run || 'false' }}"
            echo ""
            echo "See job logs for per-source detail."
          } >> "$GITHUB_STEP_SUMMARY"
```

- [ ] **Step 2: Lint locally**

Run: `actionlint .github/workflows/backlog-generate.yml` if `actionlint` is installed (skip if not). Otherwise visually verify YAML.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/backlog-generate.yml
git commit -m "ci: daily backlog-generate workflow (14:00 UTC)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 14: Budget block + ops scripts

**Files:**
- Modify: `.claude/budget.toml`
- Create: `scripts/rotate-backlog-token.sh`
- Create: `scripts/backlog-rollback.sh`

- [ ] **Step 1: Read current `.claude/budget.toml`**

Run: `cat .claude/budget.toml`

- [ ] **Step 2: Append the `[backlog_generator]` block**

If the file does not contain `[backlog_generator]`, append:

```toml
[backlog_generator]
# Per-run cap. Tier 1+2 are deterministic ($0). Tier 3 (LLM) is capped at $0.50.
per_run_usd_cap = 0.50
# Weekly aggregate ceiling (cap × 7 days).
weekly_usd_cap = 3.50
```

- [ ] **Step 3: Write `scripts/rotate-backlog-token.sh`**

```bash
#!/usr/bin/env bash
# Rotate the TT_API_BACKLOG_TOKEN GHA secret. Calls tt-api admin endpoint to
# mint a new scoped token, prints it for the operator to paste into the
# GitHub Secrets UI, and revokes the old one.
set -euo pipefail

: "${TT_API_BASE_URL:?must be set}"
: "${TT_ADMIN_TOKEN:?must be set — top-level admin token, not the backlog one}"

OLD_TOKEN_PREFIX="${1:-}"

new=$(curl -fsS -X POST \
  -H "Authorization: Bearer ${TT_ADMIN_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"scope":"/v1/admin/backlog/*","ttl_days":90}' \
  "${TT_API_BASE_URL}/v1/admin/tokens/mint" | jq -r '.token')

echo "NEW TOKEN: ${new}"
echo "Paste this into the GitHub repo secret TT_API_BACKLOG_TOKEN, then run:"
echo "  $0 ${new:0:8}  # to revoke any old tokens with that prefix"

if [[ -n "${OLD_TOKEN_PREFIX}" ]]; then
  curl -fsS -X DELETE \
    -H "Authorization: Bearer ${TT_ADMIN_TOKEN}" \
    "${TT_API_BASE_URL}/v1/admin/tokens/by-prefix/${OLD_TOKEN_PREFIX}"
  echo "Revoked tokens matching prefix ${OLD_TOKEN_PREFIX}"
fi
```

Then `chmod +x scripts/rotate-backlog-token.sh`.

- [ ] **Step 4: Write `scripts/backlog-rollback.sh`**

```bash
#!/usr/bin/env bash
# Close generator-opened issues from the last N days that have a given label.
# Use after a noisy Tier-3 run.
set -euo pipefail

LABEL="${1:?usage: backlog-rollback.sh <label> [days=1]}"
DAYS="${2:-1}"

since=$(date -u -v-"${DAYS}"d +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u --date="-${DAYS} days" +%Y-%m-%dT%H:%M:%SZ)

mapfile -t issues < <(gh issue list \
  --label "${LABEL}" \
  --state open \
  --search "created:>=${since}" \
  --json number \
  --jq '.[].number')

if [[ ${#issues[@]} -eq 0 ]]; then
  echo "No ${LABEL} issues opened since ${since}."
  exit 0
fi

echo "About to close ${#issues[@]} issue(s):"
printf '  #%s\n' "${issues[@]}"
read -rp "Continue? [y/N] " ok
[[ "${ok}" == "y" || "${ok}" == "Y" ]] || { echo "aborted"; exit 1; }

for n in "${issues[@]}"; do
  gh issue close "${n}" --reason "not planned" \
    --comment "Auto-closed by scripts/backlog-rollback.sh — generator noise"
done
```

Then `chmod +x scripts/backlog-rollback.sh`.

- [ ] **Step 5: Commit**

```bash
git add .claude/budget.toml scripts/rotate-backlog-token.sh scripts/backlog-rollback.sh
git commit -m "ops: backlog generator budget + rotate/rollback scripts

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 15: `backlog-curator` subagent placeholder

**Files:**
- Create: `.claude/agents/backlog-curator.md`

- [ ] **Step 1: Write the subagent definition**

```markdown
---
name: backlog-curator
description: Use when the Tier-3 LLM-proposal source (see Track F Tier-3 plan) needs creative ideation over a telemetry summary. NOT invoked by Tier 1 day-0 paths.
model: claude-sonnet-4-6
tools: Read, Grep, Glob, Bash
---

You are the Tier-3 backlog curator for TokenTrimmer's self-driving autopilot.

You receive a JSON telemetry summary (top expensive prompts, top cache misses, trust-score outliers, recent commits). You return at most 3 candidate backlog items as JSON matching the BacklogCandidate schema in `crates/cli/src/backlog/types.rs`.

Hard rules:

1. Confidence is your honest 0.0–1.0 self-rating. Items below 0.7 are dropped silently — be honest, don't round up.
2. Every item must cite ≥ 1 evidence URL from the telemetry summary, or a specific commit SHA / file:line.
3. `signal_id` is `propose:<suggested_subagent>:<sha256(sorted(evidence_urls))>` — paraphrases of the same work must collapse to the same id.
4. Title prefix is `proposal`.
5. Stay under $0.50 in model spend for the run. The orchestrator enforces this, but write tightly.

You do NOT write code. You return JSON only.

Full spec: `docs/superpowers/specs/2026-05-28-self-driving-backlog-design.md` §5 (Tier 3) and §7 (Safety).
```

- [ ] **Step 2: Commit**

```bash
git add .claude/agents/backlog-curator.md
git commit -m "feat(agents): backlog-curator subagent placeholder for Tier 3

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 16: Documentation + final verification

**Files:**
- Modify: `.claude/CONTEXT_MAP.md` (add backlog-generator entry)
- Modify: `AGENTS.md` (mention new subcommand)

- [ ] **Step 1: Add context-map entry**

In `.claude/CONTEXT_MAP.md`, under the **Domains** table, add a new section:

```markdown
### Autopilot backlog generator

| If you're doing | Read | Why |
|---|---|---|
| Adding a new signal source | `crates/cli/src/backlog/sentry.rs` (worked example), `docs/superpowers/specs/2026-05-28-self-driving-backlog-design.md` §5 | Trait shape: `pub async fn fetch(...) -> Result<Vec<BacklogCandidate>, _>`. |
| Changing dedupe logic | `crates/cli/src/backlog/dedupe.rs`, `crates/cli/src/backlog/issue.rs::fingerprint` | Fingerprint is the dedup primitive; embedding sim ships with Tier 2/3 plan. |
| Tightening rate limits | `crates/cli/src/backlog/rate_limit.rs::Limits` | Hard caps per spec §7.1. |
| Wiring a new audit category | `crates/cli/src/backlog/audit.rs` | All audit emit goes through tt-api. |
```

- [ ] **Step 2: Add a one-liner to `AGENTS.md`**

In `AGENTS.md` under the "Autonomous loop" section, append:

```markdown
- **Backlog generator**: `tt backlog generate` (CLI) or `.github/workflows/backlog-generate.yml` (daily 14:00 UTC). Spec at `docs/superpowers/specs/2026-05-28-self-driving-backlog-design.md`. Tier 1 only at day 0; Tiers 2 and 3 land via follow-up plans.
```

- [ ] **Step 3: Run the full local CI gate**

Run:
```bash
cargo fmt --check
cargo clippy -p tt-cli -- -D warnings
cargo test -p tt-cli
./scripts/tt-inspect-self.sh
```

Expected: all four pass with zero new findings.

- [ ] **Step 4: Commit**

```bash
git add .claude/CONTEXT_MAP.md AGENTS.md
git commit -m "docs: register backlog generator in context map + agents.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Mark backlog item complete**

In `.claude/BACKLOG.md`, change the `[ ]` for `trackF-self-driving-backlog` to `[x]` and append the shipped date marker per existing convention.

Then commit:

```bash
git add .claude/BACKLOG.md
git commit -m "backlog: trackF Day-0 MVP shipped

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Spec coverage check

| Spec section | Covered by |
|---|---|
| §4 architecture diagram | Tasks 1–11 build it; Task 13 wires the GHA cron |
| §5 signals — Sentry | Task 5 |
| §5 signals — drift/anomaly/latency | Task 6 |
| §5 signals — inspect-self | Task 7 |
| §5 signals — backlog.sh-audit | Task 8 |
| §5 signals — gh-triage Tier 2 | DEFERRED to follow-up plan |
| §5 signals — llm-propose Tier 3 | DEFERRED to follow-up plan |
| §5 signals — Resend inbound | DEFERRED per spec §9 Day 60 |
| §6 issue body schema + title | Task 3 |
| §7.1 rate limits | Task 9 (per-source caps) + Task 11 (global caps in orchestrator) |
| §7.2 dedupe primary (fingerprint) | Task 3 + Task 9 |
| §7.2 dedupe secondary (embedding) | DEFERRED (only matters with paraphrase-prone Tier 2/3) |
| §7.3 cost discipline | Task 14 budget block; Tier-1-only path is $0 |
| §7.4 confidence floor | DEFERRED to Tier 3 plan (Tier 1+2 are confidence = 1.0) |
| §7.5 subagent + model tier | Task 15 (placeholder); Tier 3 plan fleshes out |
| §7.6 failure modes — graceful degrade | Task 11 (each source try/catch independently) |
| §8 testing | Tasks 2–12 each include `#[cfg(test)]`; Task 12 integration test |
| §9 rollout — Day 0 | Tasks 1–14 |
| §10 observability — cost-ledger | DEFERRED to Tier 3 plan (Tier 1+2 has no cost) |
| §10 observability — audit row | Task 10 + Task 11 (wired through orchestrator) |
| §10 observability — GHA job summary | Task 13 |
| §11 open questions | not implemented — flagged as decisions for post-ship |

**Spec-derived files all have an owning task. No gaps in Day-0 MVP scope.**

---

## Plan complete

Plan saved to `docs/superpowers/plans/2026-05-28-self-driving-backlog-generator.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task (mostly `rust-crate-builder` at Sonnet tier), review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using the executing-plans skill, batch execution with checkpoints for your review.

Which approach?
