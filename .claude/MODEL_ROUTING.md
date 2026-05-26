# Model routing for subagents — dogfooding

We sell cost optimization; the build process must practice it. Every subagent declares a model tier, and the Claude Code session passes `--model` accordingly when dispatching it.

This file is the source of truth. Update it when you discover a tier change is justified (and record the reason).

## The three tiers

| Tier | Model | When to use | Cost guidance |
|---|---|---|---|
| **Haiku** | `claude-haiku-4-5` | Routine code-writing inside a single file with clear spec; running a checklist; docs editing; rule fixture authoring; summarization. | <$0.20 expected per dispatch. |
| **Sonnet** | `claude-sonnet-4-6` | Most subagent dispatches: implementing a Provider adapter, building an Astro page, writing one Inspect rule, multi-file Rust edits. | $0.20–$1.50 expected per dispatch. |
| **Opus** | `claude-opus-4-7` | Architecture decisions, ambiguous specs, complex async/concurrency, plan-replay-validator (statistics + correctness reasoning), tough debugging. | >$1.50 expected per dispatch — use sparingly. |

## Per-subagent default tier

| Subagent | Default tier | Override allowed? |
|---|---|---|
| `rust-crate-builder` | Sonnet | Yes — drop to Haiku for trivial edits, escalate to Opus for hairy async. |
| `provider-adapter-author` | Sonnet | Yes — escalate to Opus for the first novel-shape adapter (Anthropic, Gemini). Subsequent OpenAI-compatible adapters can use Haiku once the pattern is set. |
| `inspect-rule-author` | Haiku | Yes — escalate to Sonnet if the rule needs tree-sitter query authoring. |
| `astro-page-builder` | Sonnet | Yes — Haiku for pure layout/styling work. |
| `plan-replay-validator` | Opus | No — correctness matters; bootstrap CI reasoning is hard. |
| `dogfood-inspect-runner` | Haiku | No — it's just running a script and summarizing. |
| `onboarding-context-loader` | Haiku | No — it's a router, not an implementer. |

## Routing protocol (parent session)

Before dispatching a subagent:

1. Read this file's table.
2. Check the issue body for a `model-override:` line. If present and justified, honor it.
3. Pass `--model <tier>` to the Agent tool. If unavailable, prefix the subagent prompt with `Use the <tier> model tier for this work.`

## When to escalate or de-escalate

**Escalate to Opus when:**
- A subagent returned a wrong answer twice on the same task.
- The task spec contains the word "design" or "decide".
- An iteration runs over its declared cost cap.

**De-escalate to Haiku when:**
- The work is purely mechanical (rename a symbol, add a derive, format a fixture).
- The work has a fully worked example to mimic.
- A previous Sonnet dispatch produced clean output on the same shape.

## Measuring the routing

`scripts/weekly-review.sh` aggregates `.claude/cost-ledger.jsonl` by session and subagent. Look for:

- Subagent type whose **average dispatch cost exceeds 2x the tier's expected range** → tier is wrong or the prompt is wrong.
- Subagent type whose dispatches are **consistently below 0.5x expected** → consider de-escalating its default tier.

## Rules our own choices satisfy

- `model-flagship-for-classification` — we don't Opus the trivial.
- `model-untested-alternatives` — we measure and tune.
- `gov-no-feature-attribution` — we tag every dispatch via the audit log so spend is attributable.
