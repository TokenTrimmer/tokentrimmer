# Decision log

ADR-style record of architectural decisions. The point of this file: **stop re-litigating closed questions**. Before changing something the decision was about, read the decision first.

Format per entry: ID, date, status, context (what problem), decision (what we chose), consequences (what this costs us), pointers (where to look in code/docs).

---

## ADR-001 — Two-repo split: public OSS + private cloud (2026-05-25)

**Status**: Adopted (Week 0)

**Context**: We're open-core (Apache 2.0). Need a clean license boundary, sandboxed contributor PRs, and the freedom to move faster on commercial code.

**Decision**: Two repos: `tokentrimmer/tokentrimmer` (public, Apache 2.0; Gateway + Inspect CLI + Plan engine + SDKs) and `tokentrimmer/cloud` (private; dashboard, billing, hosted Inspect tier 2/3, Plan hosted service).

**Consequences**:
- Type drift between Rust and TS is the biggest failure mode. As of 2026-06-12,
  `crates/ts-types` is only a placeholder and `bindings-drift-check` is not a
  real regenerate+diff guard; it compiles the placeholder and emits a warning
  until `ts-rs` generation lands.
- Cross-repo PRs slow down when a change spans both. Acceptable cost.

**Pointers**: `README.md`; plan file § "Repo Structure".

---

## ADR-002 — Apache 2.0 for the OSS core (2026-05-25)

**Status**: Adopted

**Context**: Need patent grant, contributor-friendly license, and compatibility with downstream commercial users.

**Decision**: Apache 2.0 across the public repo. `deny.toml` enforces only Apache/MIT/BSD/MPL/ISC/etc. in dependencies — no GPL contamination.

**Consequences**: Anyone can fork. Hosted-only value (semantic cache, Plan, dashboard, email, Inspect Tier 2/3) is the moat.

**Pointers**: `LICENSE`, `deny.toml`.

---

## ADR-003 — Rust toolchain pinned to 1.86 (2026-05-25)

**Status**: Adopted

**Context**: Transitive deps (`url` → `idna` → `icu_*` 2.2) require rustc 1.86. Earlier (1.83/1.85) tried, both rejected.

**Decision**: Pin `rust-toolchain.toml` to `1.86.0`. Workspace `rust-version = "1.86"`.

**Consequences**: Contributors must `rustup install 1.86.0` (rustup auto-installs on first cargo run). CI image needs 1.86+. Bumping the floor again should follow the same procedure.

**Pointers**: `rust-toolchain.toml`, `Cargo.toml` `[workspace.package]`, `README.md` § Prerequisites.

---

## ADR-004 — 10 P0 Inspect rules at launch, not 15 (2026-05-25)

**Status**: Adopted — solo-founder roadmap

**Context**: Spec lists 15 rules for v1. Solo execution slips ~30-50%; 15 rules likely produces more FPs than we can triage at alpha.

**Decision**: Ship 10 highest-confidence P0 rules at Week 14: `cache-anthropic-prompt-cache-missing`, `cache-openai-prompt-cache-eligible`, `lib-anthropic-sdk-no-cache-control`, `model-flagship-for-classification`, `model-flagship-for-extraction`, `output-no-max-tokens`, `conversation-unbounded-history`, `agent-no-termination-condition`, `config-no-agents-md`, `config-agents-md-contains-secrets`.

**Consequences**: Marketing/blog can claim "10 rules at launch, growing weekly." Each new rule lands as a PR with FP measurement, which is healthier than batch-shipping 15.

**Pointers**: `.claude/BACKLOG.md` (no rule entries until Week 14), plan file § Week 14, `docs/01-inspect-rule-catalog.md`.

---

## ADR-005 — Typst over Playwright for monthly PDFs (2026-05-25)

**Status**: Adopted — Week 22 target

**Context**: Spec had open question (§ 20.2) on PDF rendering. Three candidates: Playwright (heavy, slow, ~$50/mo render capacity), React-PDF (mediocre), Typst (Rust-native, sub-second, Apache 2.0, but at version 0.14 not 1.0).

**Decision**: Typst via `typst-pdf` crate.

**Consequences**: 0.x API may shift before our v1 — pin the crate version. Saves ~$50/mo of Chromium render capacity. Smaller image footprint in worker containers.

**Pointers**: Plan file § "Critical Research Findings" #6; `crates/worker/` (cloud repo, TBD).

---

## ADR-006 — Auth.js + magic link for v1; defer WorkOS to v1.1 enterprise (2026-05-25)

**Status**: Adopted

**Context**: Enterprise SAML is $125/connection/month at WorkOS. At solo-founder stage with no enterprise customers, that's pure cost.

**Decision**: Auth.js with Resend magic-link for Week 10 launch. SAML deferred until first design-partner conversation post-beta. When SAML lands, use WorkOS (managed) for v1.1; reconsider BoxyHQ self-hosted only at >50 enterprise connections.

**Consequences**: Enterprise tier requires Auth.js → WorkOS plumbing later (one-time cost). Magic link is a familiar pattern; most non-enterprise customers will be fine.

**Pointers**: Plan file § "Critical Research Findings" #5; `crates/auth/src/lib.rs` (API keys only — web auth lives in cloud repo).

---

## ADR-007 — Apalis on Postgres for worker queue (2026-05-25)

**Status**: Tentative (Week 14+ work)

**Context**: Spec open question (§ 20.2) on worker queue: pg-boss, Redis-based, Fly machines cron.

**Decision**: Apalis (Rust async background jobs) backed by Postgres. One less moving part — matches spec's "all coordination through database" principle.

**Consequences**: Postgres write rate must absorb job queue load alongside `request_logs`. At Scale-tier volume (~50M req/mo) we may need to split queue → separate DB or Redis.

**Pointers**: Plan file § "Open Decisions Remaining" #2.

---

## ADR-008 — OpenAI `text-embedding-3-small` for semantic cache; design swap path (2026-05-25)

**Status**: Adopted for v1

**Context**: Spec open question — OpenAI embeddings (cheap, simple, per-call) vs self-hosted BGE via Candle (zero per-call, ops burden).

**Decision**: OpenAI `text-embedding-3-small` for v1. Design the embedding pipeline to allow drop-in swap to BGE later.

**Consequences**: Recurring per-call cost (~$0.02/1M tokens). Budget alert if embedding cost per cached request exceeds $0.00015.

**Pointers**: Plan file § "Risk Register" "Semantic cache embedding model price hike".

---

## ADR-009 — S3 Object Lock for Scale/Enterprise immutable audit storage (NOT R2) (2026-05-25)

**Status**: Adopted

**Context**: Cloudflare R2 does NOT support WORM Object Lock (only "Bucket Locks", a different feature). Our audit promise for Scale/Enterprise tiers depends on immutable storage.

**Decision**: Use AWS S3 with Object Lock in Compliance mode for Scale/Enterprise audit retention. Keep R2 for non-compliance assets (PDFs, exports).

**Consequences**: +$5-20/mo per Scale/Enterprise customer in COGS. Multi-cloud dependency — manageable since R2 stays primary.

**Pointers**: Plan file § "Critical Research Findings" #1; plan file § "Storage tier escalation".

---

## ADR-010 — Single Fly region (`iad`) until $5K MRR (2026-05-25)

**Status**: Adopted

**Context**: Fly.io now bills inter-region traffic at machine rates (Feb 2026 change). Spec assumed iad/lhr/syd from launch.

**Decision**: `iad` only at launch. Add `lhr` when EU customers >20% of base. Defer `syd` until enterprise APAC demand.

**Consequences**: EU customers see ~80ms extra latency at launch. Acceptable trade-off; multi-region is a marketing claim, not a P0 feature.

**Pointers**: Plan file § "Critical Research Findings" #2.

---

## ADR-011 — File-size cap of 800 lines per .rs file (2026-05-25)

**Status**: Adopted for new/full-file edits; legacy exceptions tracked

**Context**: Large files are hard to context-load for subagents and tend to mix concerns. We want our own `prompt-bloated-system` analog at the source level.

**Decision**: `.claude/hooks/pre-edit-guard.sh` blocks full-content Write/Edit on `.rs` files that would exceed 800 lines. Same rule for AGENTS.md at 4000 tokens. Existing oversized files are legacy exceptions and must shrink over time rather than grow casually.

**Consequences**: Forces module splitting earlier than typical for new code. Older monoliths still need incremental extraction; the hook is not a repository-wide size audit.

**Pointers**: `.claude/hooks/pre-edit-guard.sh`, `AGENTS.md` § Conventions.

---

## ADR-012 — Scoped cargo only; whole-workspace builds denied in hooks (2026-05-25)

**Status**: Adopted

**Context**: Whole-workspace builds emit thousands of lines and take 30-120s. Every edit triggering a workspace build is the single biggest token waste in autonomous loops.

**Decision**: `.claude/settings.json` denies `cargo test --workspace`, `cargo build --release`, `cargo build --workspace`. Only scoped `cargo {check,test,clippy} -p <crate>` is allowed in agent sessions. CI uses workspace commands; humans can override locally.

**Consequences**: Agents must know which crate they're editing. Hook `post-edit-scoped-check.sh` resolves the crate automatically from the edited file's path.

**Pointers**: `.claude/settings.json` § permissions, `.claude/hooks/post-edit-scoped-check.sh`.

---

## ADR-013 — Cost-tier-by-default subagent model routing (2026-05-25)

**Status**: Adopted

**Context**: Dogfood our own `model-flagship-for-classification` rule on ourselves. Most subagent work doesn't need Opus.

**Decision**: Each subagent declares a default tier (Haiku/Sonnet/Opus) in its frontmatter. `dogfood-inspect-runner`, `inspect-rule-author`, `onboarding-context-loader` → Haiku. `rust-crate-builder`, `provider-adapter-author`, `astro-page-builder` → Sonnet. `plan-replay-validator` → Opus (correctness reasoning).

**Consequences**: Some subagents may produce wrong output on tasks above their tier. Mitigated by mandatory return summaries — parent can re-dispatch with escalation if needed.

**Pointers**: `.claude/MODEL_ROUTING.md`, each `.claude/agents/*.md` frontmatter `model:` field.

---

## ADR-014 — Inject AGENTS.md once per session, not every prompt (2026-05-25)

**Status**: Adopted

**Context**: Re-loading AGENTS.md every user prompt is the most common token-waste pattern in Claude Code setups.

**Decision**: `.claude/hooks/inject-agents-md-once.sh` uses a `/tmp/tt-session-<id>-loaded` sentinel; injects on first prompt, no-ops thereafter.

**Consequences**: If the session's AGENTS.md needs change mid-session, agent must read the file directly. Acceptable — uncommon case.

**Pointers**: `.claude/hooks/inject-agents-md-once.sh`, `.claude/settings.json` UserPromptSubmit hook.

---

## ADR-015 — Hash-chained audit log for all tiers (including OSS) (2026-05-25)

**Status**: Adopted

**Context**: User requirement — auditable results for every tier. Competitive moat vs Helicone/Portkey/LiteLLM (none publish tamper-evident audit).

**Decision**: BLAKE3 hash chain + per-org Ed25519 signature. Verifiable via `tt audit verify` CLI (in OSS — even self-hosters get it). Storage: Postgres rows (Free→Team), + S3 Object Lock (Scale), + customer S3 sync (Enterprise).

**Consequences**: Every state-changing endpoint must emit an audit row. Test harness must verify chain integrity on every CI run.

**Pointers**: `crates/telemetry/src/audit.rs`, plan file § "Audit Guarantees".

---

## ADR-016 — `min_machines_running = 1` for the Gateway, no scale-to-zero (2026-05-27)

**Status**: Adopted

**Context**: Fly supports `min_machines_running = 0` (scale-to-zero, machine boots on first request) and `min_machines_running >= 1` (always-on). Scale-to-zero saves ~$1.50/mo of idle compute on a `shared-cpu-2x` / 512MB instance; the cost is a ~20–30 s cold-start on the first request after idle. Cold boot of the `tt gateway` binary itself is <1 s, but the cold-start budget on Fly's side includes image pull and the machine spin-up, and a misconfigured `REDIS_URL` previously caused the whole boot to hang past the 5 s grace window (see `boot_timeout` in `cli/src/main.rs`).

**Decision**: Keep `min_machines_running = 1` in `fly.toml`. Gateway is always-on.

The Gateway's product promise is a `p50 miss < 30 ms / p50 hit < 5 ms` latency budget (spec §4). Letting a customer's first request after idle take 20–30 s makes the product feel broken at the exact moment they're evaluating it — the opposite of the "TokenTrimmer is faster than calling OpenAI directly" pitch.

The $1.50/mo idle cost is trivial vs the brand cost of a cold start. We re-evaluate only if:
- Idle cost rises (e.g., moving to a larger machine class), AND
- Traffic is bursty enough that scale-to-zero would actually save meaningful money, AND
- We have a warm-pool / preemptive boot story to keep customer cold starts <2 s.

Until all three are true, stay at 1.

**Consequences**:
- ~$1.50/mo of always-on idle compute. Acceptable.
- One machine = no automatic regional failover. ADR-010 already accepted single-region (`iad`) until $5K MRR; this is consistent.
- Deploys briefly run two machines (blue/green via Fly's release strategy) — billing barely changes.
- A hung boot (timeout in `tt-config` env load or external dependency connect) takes the whole gateway down. Mitigated by the 5 s `tokio::time::timeout` wrappers around DB + Redis connects so any one external dep can fail without crashlooping the process.

**Pointers**: `fly.toml` (`http_service` block), `crates/cli/src/main.rs` (`run_gateway` boot timeouts), HANDOFF.md (gateway deploy session narrative).

---

## How to add a decision

When you make a non-trivial call:

1. Append a new ADR at the bottom of this file (don't insert in the middle — preserve historical order).
2. Use the next ID. Include date in `YYYY-MM-DD`.
3. **Status** ∈ {Tentative, Adopted, Superseded by ADR-NNN, Reverted}.
4. Keep **Consequences** honest — write down what this costs us, not just the win.
5. **Pointers** should be enough that someone re-reading 6 months from now can find the implementation.

When a decision is reversed, mark the old one `Superseded by ADR-NNN` and write the new one — never delete history.

---

## ADR-017 — No encryption-at-rest for L2 cached responses; per-org opt-out instead (2026-05-30)

**Status**: Adopted

**Context**: PROJECT_REVIEW.md §5.10 flagged that the L2 semantic cache stores responses unencrypted. The L2 table `cache_entries` (migration `0002_cache_entries`) persists per row: `embedding vector(1536)`, `response JSONB`, `model`, token counts, TTLs. By design (migration 0002 header) it NEVER stores the original prompt text — only its embedding. L1 (Redis, `redis_impl.rs` `set_ex`) is short-TTL/ephemeral; the durable at-rest surface is the L2 Postgres table. We already run a per-org AEAD scheme (XChaCha20-Poly1305, per-row key = `SHA-256(TT_MASTER_KEY ‖ domain ‖ org_id)`, AAD bound to org_id, nonce-prefixed) for genuinely-secret recoverable data in `crates/auth/src/credentials.rs` and `crates/retrieval/src/audit.rs`.

**Decision**: Do NOT encrypt the L2 `response` column at rest in v1. Instead (1) document the residual threat (this ADR) and (2) ship a per-org opt-out: orgs with sensitive payloads can disable semantic caching entirely so nothing of theirs is persisted. The opt-out is tracked as `rv-l2-org-cache-optout`.

**Consequences**:
- The high-value exfil concern is the **embeddings** (a leaked embedding can be partially inverted to recover prompt semantics). Encrypting only `response` leaves `embedding` plaintext, and `embedding` cannot be encrypted without destroying the HNSW cosine index (0002) that is the entire reason L2 exists — so response-only encryption buys little against the stated threat.
- Response-only encryption stays cheap to add later (reuse `retrieval/audit.rs` verbatim, `response JSONB` → `BYTEA`, domain separator `tt-cache:l2_response:v1`) but would add a decrypt on every cache hit (hot path, `chat.rs` `build_hit_l2_response`) and join the `TT_MASTER_KEY` rotation set (see `docs/SECRETS.md`).
- Residual threat accepted: a DB-at-rest snapshot/backup leak or staff read of `cache_entries` yields per-org embeddings + cached responses (NOT raw prompts); org_id scoping keeps it per-tenant attributable. Out of scope: cross-tenant read at query time (lookup is `WHERE org_id = $1`), live SQLi (parameterized). Mitigated by: no prompt text stored, Fly Postgres disk encryption, and the per-org opt-out.
- Revisit triggers: a contract requires column-level encryption beyond disk encryption, or responses become recoverable secrets (e.g. caching tool outputs with PII).

**Pointers**: `crates/core/migrations/0002_cache_entries.up.sql`; `crates/cache/src/l2.rs`; `crates/retrieval/src/audit.rs` (reusable AEAD); follow-up `rv-l2-org-cache-optout` (per-org `semantic_cache_disabled` → force `cache_behavior.do_lookup=do_insert=false`, resolve alongside tier in `tier_resolver.rs`, gate before the L1/L2 branches in `chat.rs`).

## ADR-018 — v1 routing is same-provider only (2026-06-04)

**Status**: Adopted (formalizes a constraint that was already enforced in code).

**Context**: The routing engine rewrites a request's `model` to a cheaper/target model. Cross-provider rewrites (e.g. `gpt-4o` → a Gemini model) need each provider's pricing reconciled before `tt_plan_core` can project savings honestly, and they change the credential/capability resolution path. The cloud create/patch handlers already reject cross-provider rewrites in `routes_admin.rs::validate_same_provider` (via `tt_shared::providers::infer_provider` / `known_to_differ`). This constraint was referenced in code and error messages as "ADR-007", but ADR-007 is actually the Apalis worker-queue decision — the routing constraint was never written down. This ADR records it under its correct number.

**Decision**: v1 routing (including the V3a content-type slice) requires the target model to be on the **same provider** as the source. Cross-provider routing is a later slice (V3 roadmap), gated on unified cross-provider pricing in `tt_plan_core`.

**Consequences**:
- Content-type routes (`has_images`/`has_audio`, added in V3a-1) pick a same-provider model of the required capability; the runtime capability guard (`chat.rs` `apply_routing`) still skips a target lacking the capability.
- The `tt_routing::RouteAction::target_model` doc comment is corrected from "ADR-007" to "ADR-018" in this change. The `cloud` error-message string (`routes_admin.rs:59`) and the cloud `HANDOFF.md` reference are updated in the V3a-2 (cloud) plan, since they live in the cloud repo.
- Revisit trigger: cross-provider pricing is unified in Plan, enabling honest cross-provider savings projection.

**Pointers**: `crates/routing/src/lib.rs` (`RouteAction::target_model`); `cloud/crates/api/src/routes_admin.rs::validate_same_provider`; `crates/shared/src/providers.rs`; roadmap `docs/superpowers/specs/2026-06-03-cli-platform-roadmap.md` (V3 cross-provider slice).
