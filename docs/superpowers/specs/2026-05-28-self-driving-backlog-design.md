# Self-driving autopilot backlog generator — design spec

**Track:** F (of six-track expansion: A=MCP, B=Claude Code/Codex proxy, C=Cost preview, D=`tt init` installer, E=RAG, F=this)
**Status:** Draft 1 — pending user review
**Author:** Claude (Opus 4.7) via brainstorming session
**Date:** 2026-05-28
**Owner:** solo founder (Ian)

---

## 1. Problem

TokenTrimmer's autopilot loop (`scripts/ralph-iteration.sh` + `.claude/AUTOPILOT_PROMPT.md`) consumes pre-written backlog items from `.claude/BACKLOG.md` / GitHub Issues labeled `autopilot`. When the human stops adding items, autopilot stops. The business goal is "runs itself" — that requires a closed loop where the backlog refills from observable signals (telemetry, errors, drift) and curated human input, with a separate review queue for LLM-proposed creative items.

## 2. Goals (non-stretch)

1. Backlog never empties as long as the system or its users produce signal.
2. Every generated item is traceable to ≥ 1 piece of evidence (Sentry event, dashboard URL, line number, telemetry summary).
3. Solo reviewer can triage the day's output in < 10 minutes.
4. Per-day generator cost ≤ $0.50; per-week ≤ $3.50.
5. Tier-1 (defects) and Tier-2 (user signal) items have zero LLM cost.
6. Failures in any single signal source degrade gracefully — other sources still ship items.

## 3. Non-goals

- Tier-4 auto-promotion (LLM-proposed items skipping human review). Re-evaluate after 30 days of clean Tier-3 behavior.
- Backlog generation for the sibling `tokentrimmer-cloud` private repo. This spec covers the public OSS repo only; cloud-repo signals are surfaced via two admin HTTP endpoints, not by direct DB access.
- Replacing human product judgment on roadmap. Tracks A–E remain in human hands.

## 4. Architecture

```
.github/workflows/backlog-generate.yml  (cron: 14:00 UTC daily)
        │
        ▼
tt backlog generate  (new CLI subcommand in crates/cli/)
        │
        ├──► Sentry HTTP API                       ──► Tier 1 (defect)
        ├──► tt-api: /v1/admin/backlog/signals     ──► Tier 1 (drift/anomaly/regression)
        ├──► tt inspect (on this repo)             ──► Tier 1 (new HIGH/CRITICAL findings)
        ├──► gh issue list (unlabeled)             ──► Tier 2 (triaged user signal)
        └──► tt-api: /v1/admin/backlog/telemetry-summary
                                                   ──► Tier 3 (LLM proposes; Sonnet, $0.50 cap)
        │
        ▼
Deduplicator (gh issue body fingerprint search + L2 embedding similarity)
        │
        ▼
gh issue create  (label: autopilot | autopilot-triaged | autopilot-proposed)
        │
        ▼
ralph-iteration.sh  (unchanged — picks --label autopilot only)
```

### 4.1 New artifacts

| Artifact | Path | Approx LOC |
|---|---|---|
| GHA workflow | `.github/workflows/backlog-generate.yml` | 60 YAML |
| CLI module | `crates/cli/src/backlog/mod.rs` + `sentry.rs` `signals.rs` `inspect.rs` `triage.rs` `propose.rs` `dedupe.rs` | ~800 Rust |
| Subagent | `.claude/agents/backlog-curator.md` | 80 markdown |
| Cloud admin endpoint | `tokentrimmer-cloud` `GET /v1/admin/backlog/signals` | ~200 Rust |
| Cloud admin endpoint | `tokentrimmer-cloud` `GET /v1/admin/backlog/telemetry-summary` | ~150 Rust |
| Cloud admin endpoint | `tokentrimmer-cloud` `POST /v1/admin/backlog/audit-run` | ~80 Rust |
| Rotation script | `scripts/rotate-backlog-token.sh` | 30 bash |
| Rollback script | `scripts/backlog-rollback.sh` | 40 bash |
| Budget line item | `.claude/budget.toml` `[backlog_generator]` block | 5 toml |

### 4.2 Boundary respect

- The OSS CLI talks to cloud-repo only through scoped admin HTTP endpoints. No DB credentials in GHA secrets.
- Admin token is read-only and scoped to paths matching `/v1/admin/backlog/*` (existing tt-api admin middleware already supports route prefixes).
- Token rotation via `scripts/rotate-backlog-token.sh`; rotation cadence: 90 days.
- Sentry token is scoped to the `tokentrimmer` project, read-only.

## 5. Signals

| Tier | Source | Trigger condition | Issue label | Cost |
|---|---|---|---|---|
| 1 | Sentry API `GET /events?level=error&seen>last_run` | New error grouping, ≥ 3 occurrences in 24h | `autopilot` | $0 |
| 1 | tt-api `/signals` — reconciliation drift | Any org with `\|projected − actual\| / projected > 0.02` on yesterday's `plan_runs` | `autopilot` | $0 |
| 1 | tt-api `/signals` — anomaly | `detectSpendAnomaly` result > 3σ for the last 24h | `autopilot` | $0 |
| 1 | tt-api `/signals` — latency regression | `latency-smoke` p50 miss > 30ms or p50 hit > 5ms for 2 consecutive runs | `autopilot` | $0 |
| 1 | `tt inspect` on this repo | New HIGH/CRITICAL finding vs `main` baseline | `autopilot` | $0 |
| 1 | `backlog.sh` audit | Items containing `[BLOCKED — needs human]` older than 7 days | `autopilot-triaged` | $0 |
| 2 | `gh issue list` unlabeled | Body matches `bug:` / `feature:` / `docs:` pattern; cheap regex first, LLM only for ambiguous | `autopilot-triaged` | $0–$0.05 |
| 2 | Resend inbound webhook to `feedback@tokentrimmer.com` *(deferred)* | Email body extracted to issue body | `autopilot-triaged` | $0 |
| 3 | tt-api `/telemetry-summary` | Sonnet reads JSON summary: top 5 expensive prompts, top 5 cache misses, trust-score outliers, top 5 recent commits; proposes ≤ 3 items/run | `autopilot-proposed` | $0.30 expected, $0.50 cap |

Day-one ships: Sentry + tt-api signals + inspect-self + Tier 2 gh triage + Tier 3 LLM. Resend inbound is deferred (requires DNS work).

## 6. Issue body schema

```markdown
**Source:** {sentry|signals.drift|signals.anomaly|signals.latency|inspect-self|gh-triage|llm-propose}
**Tier:** {1|2|3}
**Confidence:** {1.0 for Tier 1/2 deterministic, 0.0–1.0 for Tier 3 LLM self-rating}
**Suggested subagent:** {rust-crate-builder|provider-adapter-author|inspect-rule-author|astro-page-builder|plan-replay-validator|backlog-curator}
**Est cost:** ${0.00}
**Generated:** {ISO-8601}
**Run ID:** {commit-sha-of-generator-run}

### Why this matters
{1–3 sentences explaining the user/business impact}

### Evidence
{links to Sentry issue, dashboard URL, line numbers in inspect output, etc. — Tier 3 must cite ≥ 1 piece of telemetry evidence or the item is rejected pre-post}

### Suggested approach
{brief sketch — Tier 1/2 templated from the rule/source; Tier 3 LLM writes 2–4 bullets}

### Acceptance criteria
- [ ] {specific, testable}
- [ ] {specific, testable}
- [ ] inspect-self clean (no new HIGH/CRITICAL)
- [ ] cargo test green on {crate}

---
<!-- backlog-gen-fingerprint: {sha256 of source+signal-id, used by dedupe} -->
```

### 6.1 Title format

`[autopilot] {tier-prefix}: {one-line summary}`

Examples:
- `[autopilot] defect: Sentry sentry-events-payload-too-large recurring 47x/day`
- `[autopilot] drift: org_abc reconciliation 4.7% delta on 2026-05-27`
- `[autopilot] proposal: add Anthropic prompt-caching telemetry breakdown to /cache`

## 7. Safety

### 7.1 Rate limits (hard caps in `tt backlog generate`)

| Lever | Limit | Reason |
|---|---|---|
| Total issues opened per run | ≤ 10 | Reviewer attention budget |
| Tier 3 (LLM-proposed) per run | ≤ 3 | Most expensive review slot |
| Tier 2 (gh triage) per run | ≤ 5 | Existing inbox load bounded |
| Tier 1 per source per run | ≤ 5 | Avoids Sentry storm flooding |
| Active `autopilot-proposed` open at any time | ≤ 10 | Forces triage before more |
| Active `autopilot` (approved, unstarted) | ≤ 20 | Avoids deep approved queue |

### 7.2 Deduplication

Primary: `backlog-gen-fingerprint:` HTML-comment hash in issue body.

```
gh issue list --state all --search "backlog-gen-fingerprint:{sha} in:body" --json number
```

Any hit in any state (open, closed, merged) → skip. Survives label changes, reopens, comment-only edits.

**Per-source signal-id (what gets hashed):**

| Source | signal-id construction |
|---|---|
| `sentry` | `sentry:{sentry_issue_id}` (Sentry's stable grouping id) |
| `signals.drift` | `drift:{org_id}:{plan_run_date_iso}` |
| `signals.anomaly` | `anomaly:{org_id}:{hour_bucket_iso}` |
| `signals.latency` | `latency:{percentile}:{cache_layer}:{date_iso}` |
| `inspect-self` | `inspect:{rule_id}:{file_path}:{line_no}` |
| `backlog.sh-audit` | `blocked:{task_id}` |
| `gh-triage` | `gh-triage:{source_issue_number}` |
| `llm-propose` | `propose:{suggested_subagent}:{sha256(sorted(evidence_urls))}` |

Fingerprint = `sha256(source + ":" + signal-id)`. Same drift on the same org on the same day collapses to one issue, but a recurring drift on the next day opens a new one (the dedupe horizon matches the cadence at which the signal recurs).

Secondary: L2 embedding similarity check (`text-embedding-3-small`, cosine ≥ 0.88) against open-issue bodies catches fingerprint disagreement on same work. Cost ~$0.0001/run.

### 7.3 Cost discipline

| Run cost component | Budget | Enforcement |
|---|---|---|
| Tier 1+2 (deterministic) | $0.00 | No LLM calls |
| Tier 3 LLM (Sonnet, one prompt) | $0.30 expected, $0.50 hard cap | `tt backlog generate --max-cost-usd 0.50`; defaults to skipping Tier 3 |
| Embedding dedupe | $0.001 | OpenAI `text-embedding-3-small` for ≤ 20 candidates |
| Per-run total | $0.50 hard cap | GHA step fails if exceeded; alert via job summary |
| Per-week aggregate | $3.50 ($0.50 × 7) | `.claude/budget.toml` line; visible in `make weekly-review` |

If Tier 3 skipped due to cost, Tier 1+2 still ship — safe items always run.

### 7.4 Confidence floor

LLM self-rates each Tier 3 proposal `confidence: 0.0–1.0`. **Initial floor on Tier 3 ship-day (rollout Day 14): 0.7.** Items below floor are dropped silently (logged in GHA step summary, not posted). System prompt explicitly tells the LLM the floor exists and that honest uncertainty is rewarded. Floor is a workflow input read from `.github/workflows/backlog-generate.yml`; adjust via PR. See §9 for the planned trajectory.

### 7.5 Subagent + model tier

- **New subagent: `backlog-curator`** at Sonnet tier (`claude-sonnet-4-6`). Only invoked for Tier 3 ideation. Lives at `.claude/agents/backlog-curator.md`.
- Deterministic tiers (1, 2) run as plain CLI logic — no Agent dispatch, no model cost.
- Override via env: `TT_BACKLOG_CURATOR_MODEL=haiku` to downgrade after pattern is set.

### 7.6 Failure modes

| Failure | Behavior | Recovery |
|---|---|---|
| Sentry API down | Skip Tier-1-Sentry source; log; continue | Self-heals next run |
| tt-api `/signals` 5xx | Skip Tier 1 drift/anomaly/regression; log; continue | Self-heals next run |
| GitHub rate limit | Bail before opening any issue (atomicity) | Halve next-run rate limits via `--rate-limit-multiplier 0.5` |
| LLM call fails | Skip Tier 3; emit Tier 1+2 | Self-heals next run |
| Duplicate detection false-negative | Job summary tracks dupe-pair-found-after-the-fact via post-run audit | Reduce embedding cosine threshold from 0.88 → 0.85 |
| 10 garbage Tier-3 items in one day | `scripts/backlog-rollback.sh --days 1` closes all `autopilot-proposed` opened by `tokentrimmer-bot` in 24h | Tighten confidence floor to 0.8 |
| GHA secret leak | Token is scoped to read-only `/v1/admin/backlog/*`; rotation script in `scripts/rotate-backlog-token.sh` | Rotate immediately, audit `audit_log` for token use |

## 8. Testing

| Layer | Tests |
|---|---|
| Unit (Rust) | `tt backlog generate` against canned JSON fixtures per source; `insta` snapshots |
| Integration | Synthetic Sentry + tt-api via `httpmock`; verify each tier path opens correct labels |
| GHA dry-run | `--dry-run` flag prints would-open issues; required CI: dry-run produces stable output run-to-run |
| Self-replay | Replay last 30 days of real Sentry + telemetry through generator in weekly CI; assert ≤ 1σ variance in issue-open count |
| Cost gate | GHA asserts per-run cost from `.claude/cost-ledger.jsonl` < $0.50; fail if exceeded |
| Dedup gate | Run generator twice in dry-run on same data; second run must open zero issues |
| Inspect-self gate | New code passes `./scripts/tt-inspect-self.sh` (mandatory per `AUTOPILOT_PROMPT.md`) |

## 9. Rollout

1. **Day 0** — Ship Tier 1 only (Sentry + signals + inspect-self + `backlog.sh-audit`). Verify zero-cost path works for 7 days. No Tier 2 (gh-triage) or Tier 3 (LLM-propose) items yet. The `backlog.sh-audit` source ships at `autopilot-triaged` label because BLOCKED items need human unblock, not autopilot retry.
2. **Day 7** — Enable Tier 2 (gh issue triage). LLM-augmentation for ambiguous gh issues is OFF; regex-only path.
3. **Day 14** — Enable Tier 3 (LLM proposals). Confidence floor 0.7. Also flip Tier 2 LLM-augmentation ON if Day 7–13 produced < 5 false-positive triages.
4. **Day 30** — Review: drop confidence floor to 0.6 if Tier 3 quality holds (≥ 50% acceptance rate); tighten to 0.8 if noisy (< 25% acceptance rate).
5. **Day 60** — Decide whether to enable Resend inbound webhook (Tier 2 augmentation). Requires DNS + Resend domain verification.
6. **Day 90** — Reconsider Tier 4 (auto-promotion) only if 30+ days of clean Tier 3 behavior at floor 0.6 with no rollbacks.

## 10. Observability

Every run writes:
- One line appended to `.claude/cost-ledger.jsonl` (`category: "backlog_generator"`) via a final commit step in the GHA workflow.
- GHA job summary table: issues opened per tier, dedup hits, sources skipped, total cost.
- Audit row written via authenticated `POST /v1/admin/backlog/audit-run` to tt-api with body `{ run_id, opened, deduped, cost_usd, skipped_sources }`. The OSS CLI has no audit chain access; the cloud tt-api appends the row to its hash-chained audit log under event type `backlog.generator.run`.
- Failed runs surface on `make weekly-review` via parsing `.claude/cost-ledger.jsonl`.

## 11. Open questions to revisit post-ship

- Should Tier 2 LLM-augment ambiguous gh issues at all? (Currently: yes, capped at $0.05/run.) → Revisit at day 14.
- Should we expose `tt backlog generate --tier 3 --dry-run` as a `make` target for human spot-check before each shipped change? → Probably yes; defer to plan.
- Does `backlog-curator` need its own audit-log category, or roll up under `autopilot`? → Plan-level decision.

## 12. References

- Existing autopilot loop: `.claude/AUTOPILOT_PROMPT.md`, `scripts/ralph-iteration.sh`
- Existing backlog protocol: `.claude/BACKLOG.md`, `scripts/backlog.sh`
- Existing audit emit: `crates/telemetry/src/audit/mod.rs`
- Existing model routing: `.claude/MODEL_ROUTING.md`
- Existing cost cap: `.claude/hooks/cost-cap-check.sh`, `.claude/budget.toml`
- Reconciliation source: `tokentrimmer-cloud` `crates/api/src/reconciliation.rs` (private repo)
- Anomaly source: `tokentrimmer-cloud` `lib/anomaly.ts::detectSpendAnomaly` (private repo)
