# TokenTrimmer CLI + Platform Roadmap (V0–V7)

_Date: 2026-06-03 · Status: living roadmap · Owner: @iansimon_

> **Why this doc exists:** the 6-idea brainstorm below was decomposed into a
> versioned roadmap during a working session, but only the **V0** sub-project was
> written to disk (`2026-06-03-v0-cli-foundation-design.md` + its plan). The rest
> of the roadmap lived in a conversation that was later `/clear`ed. This file
> re-persists the whole map so it is never lost again. **Each `Vn` below becomes
> its own spec + plan in `docs/superpowers/{specs,plans}/` when it's picked up.**

## The original 6 ideas (verbatim intent)

1. **`tt login` from a browser** — log in via the CLI so a `TT_API_KEY` isn't
   required on every request; grab the key from the logged-in session.
2. **Much richer routes** — rules that can be simple *or* complex: "always route
   model X → Y"; "route image-gen one model, video another, image-processing a
   third"; "route legally-sensitive / privacy-required prompts to a **local**
   model"; preference dimensions **beyond token totals**.
3. **Prompting interfaces** — a prompt UI in the dashboard that uses the routes,
   and a `tt chat` in the CLI that routes *and* renders responses like Claude
   Code / codex. Lean on compatibility (base-URL override) and find a way to
   **save tokens for monthly-plan users too**. Is the MCP server enough, or does
   it need to grow?
4. **Always-current model/provider list** — so users never guess provider names
   or the exact upstream model id (e.g. `gpt-5.4-mini`) when configuring.
5. **Better `tt init` / `tt audit`** — repo-aware, AI-assisted: generate guided
   docs; have a model review the repo + audit results and weigh in; then address
   audit items *through* routed `tt` requests (save the user money).
6. **Graphically refresh the CLI** — it feels dated; modern look & feel, friendly.

---

## Current state (verified against the code 2026-06-03)

| Area | What exists today | Key files |
|------|-------------------|-----------|
| **Credentials/config** | Resolved ad-hoc in 2 spots; no store. **V0 is fixing this.** | `crates/cli/src/main.rs` (Mcp/Proxy arms), new `crates/cli/src/context/` |
| **Routing** | Real engine: `Route{when:{model_in,input_tokens_lt/gt,tag_equals}, then:{target_model,fallbacks}}`, priority-ordered, per-org. Postgres `routes` table + dashboard `/routes` (raw-JSON editing). **Same-provider only (ADR-007)**, enforced in `routes_admin.rs::validate_same_provider`. Applied in `apply_routing()` before cache, with a capability guard. | `crates/routing/src/lib.rs:33-168`; `crates/core/src/routes/chat.rs:1592-1667`; `cloud/crates/api/src/routes_admin.rs:39-64`; `cloud/crates/api/migrations/0002_routes.up.sql`; `cloud/apps/dashboard/src/pages/routes/index.astro` |
| **Chat / prompting** | Gateway `POST /v1/chat/completions` is OpenAI-compatible with routing+cache+savings+SSE. `tt proxy` is a **passive** listener (no REPL). **No `tt chat`, no dashboard playground.** MCP = admin-only (5 tools, none proxy a completion). | `crates/core/src/routes/chat.rs`, `routes/sse.rs`; `crates/cli/src/proxy/`; `crates/mcp/src/tools/` |
| **Model catalog** | `pricing.toml` (data) + **hard-coded** model lists per adapter (code). `Capability{Text,Vision,Audio,Tools,JsonMode,Streaming,Reasoning,PromptCaching}` in Rust. `GET /v1/models` enumerates the registry. **No upstream auto-refresh** (OpenRouter `GET /models` is a TODO). Manual `scripts/refresh-pricing.sh`. | `crates/shared/data/pricing.toml`; `crates/shared/src/pricing.rs`, `providers.rs`; `crates/providers/*/src/lib.rs`; `crates/core/src/routes/models.rs` |
| **`tt init`** | Deterministic: detect language/framework from manifests → Tera templates → safe merge → inspect baseline. **No AI.** `init/prompts.rs` is a stub. | `crates/cli/src/init/` |
| **`tt audit`** | Today `tt audit verify` = Ed25519 hash-chain verification only. The "audit my repo for savings" the user means is closer to `tt inspect`. **No AI commentary, no auto-fix.** | `crates/cli/src/main.rs` (Audit arm); `crates/inspect-core/` |
| **CLI visuals** | No color/TUI/table/spinner libs in active use (`crossterm`+`dialoguer` present, `dialoguer` stubbed). Ad-hoc `println!`. Proxy "TUI" = hand-rolled 1-line status + box banner. | `crates/cli/src/proxy/tui.rs`; CLI output throughout `main.rs` |
| **BYO-key (monthly-plan savings)** | Per-org `ProviderCredentialStore` exists (InMemory/Env/Chained/Postgres). Env-var store in practice; **dashboard credential UI not shipped.** This is the lever for "save tokens on monthly plans" (point TT at the user's own provider key). | `crates/auth/src/credentials.rs`; `crates/core/src/routes/chat.rs:1454-1472` |

---

## The versioned roadmap

Ordering rationale: **V0** is the credential/config seam every later CLI feature
plugs into. **V1** (styling) is a cross-cutting dependency for every CLI surface,
so it lands early. After that, V2–V6 are largely independent and can be reordered
by priority.

### V0 — Shared CLI foundation `[IN PROGRESS]`
One credential/config resolution seam (flag > env > `~/.tokentrimmer/` file >
default), configurable base URL defaulting to `https://api.tokentrimmer.com`,
local store usable now via `tt login --token`, plus `tt whoami`/`tt logout` and
`.gitignore` hardening. **Spec/plan written; Tasks 1–2 committed; Tasks 3–7
remaining.** Files: `docs/superpowers/specs/2026-06-03-v0-cli-foundation-design.md`,
`docs/superpowers/plans/2026-06-03-v0-cli-foundation.md`.

### V1 — CLI design system (idea 6)
A shared `tt_cli::ui` theming module: palette, semantic styles (success/warn/
error/muted), tables, spinners/progress, boxed summaries, `--no-color`/`NO_COLOR`
+ non-TTY detection, `--json` everywhere. Adopt a vetted stack (`owo-colors`/
`anstream` + `comfy-table` + `indicatif`; `ratatui` only where a real TUI is
warranted). Refactor existing ad-hoc output (inspect/plan/init/proxy) onto it.
**Dependency for the visual quality of V2/V5/V6.**

### V2 — Browser login (idea 1)
`tt login` (no `--token`) opens a browser and runs an **OAuth device /
authorization-code-with-PKCE** flow against the gateway, stores the returned key
via the V0 store, and adds optional OS-keychain backing (`keyring`). Networked
`tt whoami` (org/email via a gateway identity endpoint) and server-side revoke on
`tt logout`. Requires a gateway auth endpoint (cloud-repo work).

### V3 — Routing overhaul (idea 2)
Extend `RouteConditions`/`RouteAction` + matcher + dashboard:
- **Simple-rule UX**: one-click "always route A→B", "pin everything to model X".
- **Content-type conditions**: `has_images`/`has_audio`/`has_video` by inspecting
  `ContentPart`; route image-gen / video / image-processing to specific models.
- **Topic/keyword conditions**: `prompt_contains_any_of` (substring first;
  semantic/classifier later) for legal/sensitive/private routing.
- **Privacy → local**: a `sensitive` signal (header or topic match) that forces a
  **local** provider (`crates/providers/local`: ollama/vllm/lmstudio) and pairs
  with the L2 cache opt-out (ADR-017).
- **Beyond tokens**: preference dimensions (latency, quality band, $ ceiling).
- **Cross-provider**: relax ADR-007 once Plan pricing is unified across providers.
Promote ADR-007 (currently only in code) into `DECISIONS.md` with the migration.

### V4 — Live model/provider catalog (idea 4)
A catalog service that fetches provider `/models` (OpenRouter first — there's
already a TODO), normalizes id + capabilities + context window + price, caches
with TTL + periodic refresh, and exposes it via `GET /v1/models` and a new
`tt models` command. Drives **autocomplete + validation** when entering routes/
config (no more guessing `gpt-5.4-mini`). Single source of truth replacing the
hard-coded per-adapter lists; deprecation flags.

### V5 — Prompting interfaces (idea 3)
- **`tt chat`**: a streaming REPL (built on V1) that sends through the gateway/
  proxy, renders responses + live cost/route/cache telemetry, keeps multi-turn
  history — Claude-Code-like, compatibility-first.
- **Dashboard playground**: a prompt page that exercises the user's routes and
  shows the chosen route + savings.
- **MCP**: admin tools are sufficient today; add a `send_chat_completion` tool
  only if we want MCP itself to be a chat surface.
- **Monthly-plan savings**: ship the **BYO-key** path end-to-end (dashboard
  credential UI + proxy passthrough) so plan users save tokens against their own
  provider key, not just API-billed users.

### V6 — AI-assisted init & audit (idea 5)
- **`tt init`**: after detection, an AI pass (via routed `tt` requests) drafts
  guided docs (AGENTS.md/CLAUDE.md, a savings playbook) grounded in the repo.
- **`tt audit`**: run inspect, then have a model review repo + findings and weigh
  in (prioritize, explain, estimate savings), then **address items through routed
  requests** (propose diffs the user can apply). Dogfoods the gateway and proves
  the savings story. (Note: distinct from today's `tt audit verify` crypto chain —
  decide naming, e.g. `tt audit run` vs `tt inspect --ai`.)

### V7 — AI client (idea 3, advanced)
A fuller agentic `tt` client (beyond `tt chat`): tools, file edits, repo actions —
all routed/trimmed through TokenTrimmer. Builds on V0+V1+V5. Compatibility with
existing agent ecosystems over raw feature count.

---

## Constraints / decisions to carry forward
- **ADR-007** (same-provider routing) is enforced in code but **not yet in
  `DECISIONS.md`** — formalize it in V3.
- **ADR-017** (no L2 encryption at rest in v1; per-org cache opt-out instead) —
  V3 privacy routing should compose with it.
- 800-line `.rs` cap (ADR-011); scoped `cargo`/`pnpm` only in agent loops
  (ADR-012); cross-repo type drift risk (regenerate `@tokentrimmer/types`).

## Next action
Finish **V0** (Tasks 3–7), then pick the next `Vn` by priority and write its
spec+plan before implementing.
