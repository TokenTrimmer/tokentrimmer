# TokenTrimmer Inspect — Rule Catalog

**Status:** Brainstorm + v1 launch selection
**Purpose:** Comprehensive catalog of every detection rule TokenTrimmer Inspect should support, organized by family, with detection logic, fix recommendations, and v1 priority tagging.

---

## Reading this catalog

Every rule has the following fields:

- **ID** — stable identifier, used in code and reports (`<family>-<short-name>`)
- **Tier** — execution mechanism: 1 (deterministic AST/regex), 2 (small specialized model), 3 (frontier LLM)
- **Severity** — `low` / `medium` / `high` / `critical`
- **Languages** — Python, TypeScript (v1); others deferred
- **Detect** — what the rule looks for, in plain language
- **Why it costs** — the mechanism by which this wastes money
- **Fix** — what TokenTrimmer recommends
- **Priority** — v1 launch tag:
  - **P0** = ship in v1 launch (target: 15 rules total)
  - **P1** = ship within 60 days of launch
  - **P2** = v2 expansion (3–6 months)
  - **P3** = long-term / research

**v1 launch criteria for P0:** Tier 1 or low-cost Tier 2, low false-positive rate proven on a fixture corpus, clear high-value fix, supports both Python and TypeScript.

---

## Family index

1. [Model selection](#1-model-selection)
2. [Prompt engineering](#2-prompt-engineering)
3. [Caching](#3-caching)
4. [Context & conversation management](#4-context--conversation-management)
5. [RAG patterns](#5-rag-patterns)
6. [Agent & tool use](#6-agent--tool-use)
7. [Output handling](#7-output-handling)
8. [Architecture](#8-architecture)
9. [AGENTS.md / CLAUDE.md / project config](#9-agentsmd--claudemd--project-config)
10. [MCP opportunities](#10-mcp-opportunities)
11. [Cost governance & monitoring](#11-cost-governance--monitoring)
12. [Provider-specific optimizations](#12-provider-specific-optimizations)
13. [Local & hybrid opportunities](#13-local--hybrid-opportunities)
14. [Library-specific anti-patterns](#14-library-specific-anti-patterns)
15. [Hidden cost sources](#15-hidden-cost-sources)

---

## 1. Model selection

**Theme:** Wrong model for the task. Highest dollar-impact family — flagship models doing trivial work is the single biggest source of waste in real codebases.

### `model-flagship-for-classification`
- **Tier:** 2 · **Severity:** high · **Priority:** **P0**
- **Detect:** Calls to GPT-4*, Claude Opus/Sonnet, or Gemini Pro where the prompt asks for classification, categorization, label assignment, or boolean answer. Detect via small-model classifier on prompt text.
- **Why it costs:** Flagship models cost 15–50× more than mini/Haiku/Flash on tasks where the small model achieves equivalent quality.
- **Fix:** Swap to Haiku, GPT-4o-mini, or Gemini Flash. Provide diff. Recommend Plan run to validate quality.

### `model-flagship-for-extraction`
- **Tier:** 2 · **Severity:** high · **Priority:** **P0**
- **Detect:** Flagship-model calls extracting structured data from short inputs (JSON, key-value pairs, named entities).
- **Why it costs:** Same as above — extraction is a "small model" task.
- **Fix:** Swap to mini/Haiku/Flash with `response_format: json_object` or equivalent.

### `model-flagship-for-formatting`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P1**
- **Detect:** Flagship-model calls where the prompt asks to reformat, restructure, or convert text (markdown → JSON, table conversion, etc.).
- **Why it costs:** Pure formatting requires no reasoning.
- **Fix:** Recommend cheap model; in some cases recommend deterministic code.

### `model-reasoning-for-non-reasoning`
- **Tier:** 2 · **Severity:** high · **Priority:** **P1**
- **Detect:** Use of `o1`, `o3`, `o1-mini`, or Claude with extended thinking on tasks that don't require reasoning (classification, simple Q&A, formatting).
- **Why it costs:** Reasoning models have 3–10× higher token consumption due to internal reasoning traces.
- **Fix:** Swap to non-reasoning variant of the same family.

### `model-hardcoded-string`
- **Tier:** 1 · **Severity:** low · **Priority:** **P1**
- **Detect:** Model name passed as a string literal at the call site rather than from a config/env variable.
- **Why it costs:** Prevents centralized model management, A/B testing, and quick swaps when pricing changes.
- **Fix:** Recommend a model configuration module pattern.

### `model-deprecated`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P0**
- **Detect:** Use of deprecated or legacy model identifiers (e.g., `gpt-3.5-turbo-0301`, `claude-1.x`, `text-davinci-003`).
- **Why it costs:** Older models are often more expensive per quality unit and may be sunset.
- **Fix:** Recommend current equivalent with pricing comparison.

### `model-mismatch-vision-text-only`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Vision-capable models used for text-only inputs.
- **Why it costs:** Vision models are typically more expensive even when no image is supplied.
- **Fix:** Swap to text-only sibling model.

### `model-uniform-tier`
- **Tier:** 3 · **Severity:** medium · **Priority:** **P2**
- **Detect:** All LLM calls in a codebase use the same model regardless of complexity.
- **Why it costs:** No complexity-aware routing means flagship is wasted on simple tasks.
- **Fix:** Recommend introducing a complexity classifier and routing layer (point at TokenTrimmer Gateway).

### `model-untested-alternatives`
- **Tier:** 3 · **Severity:** low · **Priority:** **P3**
- **Detect:** No evidence of A/B testing or eval suites comparing models for a given task.
- **Why it costs:** Teams stick with the first model they tried, often more expensive than needed.
- **Fix:** Recommend running a Plan simulation against cheaper alternatives.

---

## 2. Prompt engineering

**Theme:** Bloated, redundant, or poorly-structured prompts. Often the second-largest source of waste after model selection.

### `prompt-bloated-system`
- **Tier:** 2 · **Severity:** high · **Priority:** **P0**
- **Detect:** System prompts longer than 4,000 tokens, scored by a verbosity classifier as having low information density.
- **Why it costs:** Every request pays for the bloat. With 100K requests/month, a 1,000-token bloat costs hundreds.
- **Fix:** Show the most redundant or low-value sections; suggest compression.

### `prompt-redundant-instructions`
- **Tier:** 3 · **Severity:** medium · **Priority:** **P1**
- **Detect:** Instructions in prompts that duplicate model default behavior (e.g., "be helpful," "respond in English," "be polite").
- **Why it costs:** Wasted tokens on instructions the model already follows.
- **Fix:** Strike-through suggestions with the model's documented defaults.

### `prompt-verbose-few-shot`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P0**
- **Detect:** Few-shot examples occupying more than 50% of system prompt tokens, with redundant or near-duplicate examples.
- **Why it costs:** Examples are paid for on every call.
- **Fix:** Suggest summarized examples or moving to a fine-tuned model if example count is high.

### `prompt-stale-few-shot`
- **Tier:** 3 · **Severity:** low · **Priority:** **P2**
- **Detect:** Few-shot examples that no longer reflect current task patterns (compared against recent traffic if Gateway data available).
- **Why it costs:** Outdated examples may degrade quality and add tokens.
- **Fix:** Suggest example refresh based on recent successful outputs.

### `prompt-no-cache-prefix-stability`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** System prompts where dynamic content (timestamps, user IDs) is placed *before* static instructions, breaking prompt-cache prefix matching.
- **Why it costs:** Provider prompt caching (Anthropic, OpenAI) requires stable prefixes.
- **Fix:** Reorder so static content comes first, dynamic content last.

### `prompt-markdown-bloat`
- **Tier:** 2 · **Severity:** low · **Priority:** **P2**
- **Detect:** Heavy markdown formatting in prompts (excessive `**`, `###`, table syntax) where plain text would convey the same meaning.
- **Why it costs:** Markdown adds tokens without value when the model doesn't need formatting cues.
- **Fix:** Suggest plain-text rewrite.

### `prompt-politeness-filler`
- **Tier:** 1 · **Severity:** low · **Priority:** **P2**
- **Detect:** Prompts containing "please," "thank you," "if you don't mind," and similar filler phrases.
- **Why it costs:** Wasted tokens.
- **Fix:** Suggest removal. (Note: some users intentionally include politeness; rule defaults to "warn, don't autofix.")

### `prompt-excessive-role-context`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Long role-playing preambles ("You are a senior engineer with 20 years of experience in...") that add tokens without measurable quality lift.
- **Why it costs:** Token bloat that research suggests adds minimal value beyond a brief role assignment.
- **Fix:** Suggest condensed role line.

### `prompt-whole-document-dump`
- **Tier:** 2 · **Severity:** high · **Priority:** **P1**
- **Detect:** Entire files or documents pasted into prompts when only a section is relevant.
- **Why it costs:** Massive token waste, especially on repeated queries.
- **Fix:** Recommend RAG chunking or query-aware extraction.

### `prompt-no-output-constraint`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P0**
- **Detect:** Calls without `max_tokens` or equivalent output constraint where output is expected to be short (extraction, classification, yes/no).
- **Why it costs:** Models can produce unbounded outputs; constraining them caps cost.
- **Fix:** Add appropriate `max_tokens`.

### `prompt-duplicated-across-files`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Same or near-identical prompt text in multiple files.
- **Why it costs:** Maintenance hazard; missed optimization opportunities apply only to one copy.
- **Fix:** Recommend prompt-template module.

### `prompt-no-versioning`
- **Tier:** 1 · **Severity:** low · **Priority:** **P3**
- **Detect:** No prompt versioning system (prompts in code without version tags or in a prompt registry).
- **Why it costs:** Can't A/B test; can't roll back; can't measure regression.
- **Fix:** Recommend prompt versioning approach.

---

## 3. Caching

**Theme:** Missing caching is leaving money on the floor. Provider-native prompt caching is particularly under-used.

### `cache-anthropic-prompt-cache-missing`
- **Tier:** 1 · **Severity:** critical · **Priority:** **P0**
- **Detect:** Anthropic API calls with system prompts ≥ 1,024 tokens lacking `cache_control: {"type": "ephemeral"}`.
- **Why it costs:** Anthropic prompt caching reduces cached input cost by 90%. Missing this is the single highest-ROI fix in many codebases.
- **Fix:** Add `cache_control` block. Provide exact diff.

### `cache-openai-prompt-cache-eligible`
- **Tier:** 1 · **Severity:** high · **Priority:** **P0**
- **Detect:** OpenAI calls with static prompt prefixes ≥ 1,024 tokens that could benefit from automatic prompt caching.
- **Why it costs:** OpenAI prompt caching reduces input cost 50% but requires stable prefixes.
- **Fix:** Suggest restructure to ensure prefix stability.

### `cache-no-exact-match-layer`
- **Tier:** 3 · **Severity:** high · **Priority:** **P1**
- **Detect:** Repeated identical LLM requests with no caching layer between code and provider.
- **Why it costs:** Paying for identical answers repeatedly.
- **Fix:** Recommend adding an exact-match cache (point at TokenTrimmer Gateway).

### `cache-no-semantic-layer`
- **Tier:** 3 · **Severity:** medium · **Priority:** **P1**
- **Detect:** Chatbot or Q&A applications with no semantic cache. Detect via patterns (chat endpoints, conversation handlers).
- **Why it costs:** Paraphrased queries hit the model when a near-match exists.
- **Fix:** Recommend semantic cache layer.

### `cache-key-includes-nondeterministic`
- **Tier:** 2 · **Severity:** high · **Priority:** **P1**
- **Detect:** Cache keys that include timestamps, request IDs, or other non-deterministic values.
- **Why it costs:** Cache hit rate of zero on otherwise-cacheable requests.
- **Fix:** Show key construction and suggest stripping non-deterministic fields.

### `cache-ttl-too-short`
- **Tier:** 3 · **Severity:** low · **Priority:** **P2**
- **Detect:** Cache TTL set below the data's actual freshness requirement (e.g., 60s TTL for documentation lookups that rarely change).
- **Why it costs:** Premature eviction = wasted hits.
- **Fix:** Suggest TTL extension based on observed update frequency.

### `cache-storing-user-specific`
- **Tier:** 3 · **Severity:** critical · **Priority:** **P1**
- **Detect:** Cache keys that don't include user identity but cache responses derived from user-specific data.
- **Why it costs:** Privacy bug + wrong answers (not just cost). Critical severity because it's a correctness issue.
- **Fix:** Add user identity to cache key.

### `cache-tool-definitions-resent`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** Tool/function definitions re-sent on every call within a session instead of cached.
- **Why it costs:** Tool definitions are often hundreds of tokens.
- **Fix:** Suggest provider prompt caching of tool blocks.

---

## 4. Context & conversation management

**Theme:** Unbounded growth of context. Conversations that never get trimmed compound cost on every turn.

### `conversation-unbounded-history`
- **Tier:** 2 · **Severity:** high · **Priority:** **P0**
- **Detect:** Conversation handlers that append messages indefinitely with no pruning, summarization, or sliding window.
- **Why it costs:** Cost grows quadratically with conversation length.
- **Fix:** Recommend sliding window, summarization, or hierarchical context strategy.

### `conversation-no-summarization`
- **Tier:** 3 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Long conversation patterns with no checkpoint summarization.
- **Why it costs:** Full history sent on every turn.
- **Fix:** Recommend periodic summarization (e.g., every 20 turns, replace history with summary).

### `conversation-resends-rag-context`
- **Tier:** 2 · **Severity:** high · **Priority:** **P1**
- **Detect:** Retrieved context (RAG chunks) re-fetched and re-sent on every conversation turn even when query is similar.
- **Why it costs:** Massive redundancy.
- **Fix:** Recommend caching retrieved context per session.

### `conversation-no-sliding-window`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P1**
- **Detect:** No mechanism to drop oldest turns once context exceeds a threshold.
- **Why it costs:** Token costs grow unboundedly per conversation.
- **Fix:** Recommend sliding window with configurable max turns.

### `conversation-no-message-deduplication`
- **Tier:** 2 · **Severity:** low · **Priority:** **P3**
- **Detect:** Duplicate or near-duplicate messages in conversation history.
- **Why it costs:** Wasted tokens on repeated content.
- **Fix:** Suggest dedup at append time.

---

## 5. RAG patterns

**Theme:** Retrieval-augmented generation is a fertile cost-waste area. Wrong chunk size, wrong top-k, missing re-rank.

### `rag-top-k-too-high`
- **Tier:** 2 · **Severity:** high · **Priority:** **P0**
- **Detect:** Retrieval calls returning > 10 chunks where < 5 typically suffice (heuristic + LLM-based judgment of typical query complexity).
- **Why it costs:** Each extra chunk = more input tokens per query.
- **Fix:** Suggest top-k tuning with eval to validate quality.

### `rag-chunks-too-large`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** Chunk sizes > 1,000 tokens combined with high top-k.
- **Why it costs:** Token bloat per retrieval.
- **Fix:** Suggest smaller chunks (200–500 tokens typical sweet spot).

### `rag-no-reranking`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P1**
- **Detect:** RAG pipeline without re-ranking step (vector search → LLM, no rerank).
- **Why it costs:** Forces higher top-k for same quality, or accepts lower quality.
- **Fix:** Suggest adding a re-ranker (Cohere Rerank, BGE-reranker).

### `rag-no-query-rewriting`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** RAG with no query rewriting/expansion before retrieval.
- **Why it costs:** Worse retrieval quality often patched by sending more chunks (more tokens).
- **Fix:** Suggest lightweight query rewriting with cheap model.

### `rag-no-hybrid-search`
- **Tier:** 2 · **Severity:** low · **Priority:** **P2**
- **Detect:** RAG using only semantic search, no keyword/BM25 hybrid.
- **Why it costs:** Misses exact-match needles; teams compensate with higher top-k.
- **Fix:** Suggest hybrid search pattern.

### `rag-repeated-embedding-of-same-docs`
- **Tier:** 2 · **Severity:** high · **Priority:** **P1**
- **Detect:** Embedding generation patterns that re-embed unchanged documents.
- **Why it costs:** Embedding costs add up at scale.
- **Fix:** Recommend embedding cache with content hash.

### `rag-no-embedding-cache`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** Query embedding generation without caching for common queries.
- **Why it costs:** Repeated embedding API calls for the same query strings.
- **Fix:** Recommend embedding cache layer.

### `rag-flagship-embedding-model`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Use of `text-embedding-3-large` where `text-embedding-3-small` would suffice (typical for sub-1M-doc collections).
- **Why it costs:** Large embedding model is ~6.5× more expensive.
- **Fix:** Recommend small model with eval validation.

### `rag-no-filter-before-retrieval`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Retrieval over entire corpus when metadata filters (date, author, category) could prune dramatically first.
- **Why it costs:** Forces larger top-k for needle-in-haystack queries.
- **Fix:** Suggest metadata-aware retrieval.

---

## 6. Agent & tool use

**Theme:** Agents that loop, re-explore, or use the LLM for what code should do.

### `agent-no-termination-condition`
- **Tier:** 2 · **Severity:** critical · **Priority:** **P0**
- **Detect:** Agent loops (while True with LLM calls) without max-iteration cap or explicit termination logic.
- **Why it costs:** Runaway loops can rack up hundreds of dollars per incident. Critical for cost AND correctness.
- **Fix:** Add max iteration cap, idle detection, and budget circuit breaker.

### `agent-runaway-loop-tripwire`
- **Tier:** 1 · **Severity:** high · **Priority:** **P1**
- **Detect:** The runtime half of the termination contract — agent loops that DO
  carry a termination condition but one the code can defeat: a `continue` that
  skips the
  counter/termination update, a break/check behind a conditional that
  tool output commonly fails, or a loop-carried counter never re-assigned on the
  hot path. Ships in the Tier-1 rule pack (`all_rules()`).
- **Why it costs:** A loop whose termination bookkeeping is bypassed is
  runtime-unkillable from the inside — the cap token exists but never fires;
  the spend continues until the caller kills the process or the budget
  circuit breaker trips at the gateway.
- **Fix:** Move the iteration cap increment/until-check onto the loop's
  unconditional path (or floor it with a separate hard `for range(...)`), and
  gate the termination branch on the cap, not on a tool-success condition.

### `agent-no-budget-limit`
- **Tier:** 2 · **Severity:** high · **Priority:** **P1**
- **Detect:** Agent without per-invocation cost ceiling.
- **Why it costs:** A single misbehaving agent invocation can cost thousands.
- **Fix:** Add per-invocation budget enforcement.

### `agent-verbose-tool-descriptions`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** Tool/function descriptions > 200 tokens each, repeated across many calls.
- **Why it costs:** Tool descriptions are included in every call. Verbose × repeated = expensive.
- **Fix:** Tighten descriptions; use one-liners with examples in a separate doc if needed.

### `agent-tool-output-bloat`
- **Tier:** 2 · **Severity:** high · **Priority:** **P1**
- **Detect:** Tools returning huge outputs (full file contents, long API responses) without filtering.
- **Why it costs:** Tool outputs become input tokens for the next LLM turn.
- **Fix:** Recommend output filtering/truncation before returning to the model.

### `agent-sequential-when-parallel`
- **Tier:** 3 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Agent making sequential tool calls that have no data dependencies.
- **Why it costs:** Latency + sometimes redundant context.
- **Fix:** Suggest parallel tool invocation.

### `agent-llm-for-deterministic-task`
- **Tier:** 3 · **Severity:** high · **Priority:** **P1**
- **Detect:** LLM calls for tasks classical code does well: date parsing, math, regex matching, deterministic enum classification, string manipulation, JSON validation.
- **Why it costs:** Replacing one LLM call with three lines of code = ~100% savings on that operation.
- **Fix:** Show the equivalent code; recommend deletion of the LLM call.

### `agent-no-tool-result-caching`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Tool calls with same inputs producing same outputs not cached at the tool layer.
- **Why it costs:** Re-runs expensive operations (DB queries, API calls).
- **Fix:** Suggest tool-level memoization.

### `agent-redundant-context-passing`
- **Tier:** 3 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Multi-step agents that re-pass the same context at every step.
- **Why it costs:** Token bloat across steps.
- **Fix:** Suggest shared state store or reference-passing.

### `agent-no-planner-executor-split`
- **Tier:** 3 · **Severity:** low · **Priority:** **P3**
- **Detect:** Complex agents using flagship model for both planning and execution where a planner-executor split would let the executor be cheap.
- **Why it costs:** Flagship model doing low-complexity execution.
- **Fix:** Recommend architectural refactor.

### `agent-reasoning-bloat`
- **Tier:** 2 · **Severity:** low · **Priority:** **P3**
- **Detect:** Agent reasoning chains accumulated and re-sent on every step.
- **Why it costs:** Reasoning tokens grow with each step.
- **Fix:** Recommend reasoning truncation or summarization between steps.

---

## 7. Output handling

**Theme:** How outputs are requested and consumed affects cost beyond just `max_tokens`.

### `output-no-max-tokens`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P0**
- **Detect:** LLM calls without `max_tokens` parameter.
- **Why it costs:** Unbounded outputs.
- **Fix:** Add reasonable `max_tokens` based on inferred task type.

### `output-no-structured-format`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P1**
- **Detect:** Prompts asking for JSON/structured output without using `response_format: json_object`, JSON mode, or grammar constraints.
- **Why it costs:** Free-text outputs are longer; downstream parsing fails more often, causing retries.
- **Fix:** Switch to structured output mode.

### `output-streaming-when-batch`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Offline / background tasks using streaming where OpenAI Batch API (50% cheaper) would work.
- **Why it costs:** Missing 50% discount on non-realtime work.
- **Fix:** Recommend Batch API for the pipeline.

### `output-not-streaming-when-ux`
- **Tier:** 2 · **Severity:** low · **Priority:** **P2**
- **Detect:** User-facing chatbot endpoints not using streaming.
- **Why it costs:** Doesn't save money but degrades UX, which sometimes leads to retries from impatient users.
- **Fix:** Recommend streaming.

### `output-no-stop-sequences`
- **Tier:** 2 · **Severity:** low · **Priority:** **P2**
- **Detect:** Generation tasks where stop sequences would terminate output early.
- **Why it costs:** Extra output tokens.
- **Fix:** Recommend stop sequences.

### `output-repeated-parsing`
- **Tier:** 2 · **Severity:** low · **Priority:** **P3**
- **Detect:** Same LLM output parsed multiple times by repeated downstream calls.
- **Why it costs:** Indirect — encourages re-generation.
- **Fix:** Cache parsed output.

---

## 8. Architecture

**Theme:** System-level patterns that affect cost across the entire application.

### `arch-no-retry-backoff`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** LLM calls with retries but no exponential backoff or jitter.
- **Why it costs:** Aggressive retries hit rate limits, multiply spend on failures.
- **Fix:** Recommend backoff library usage.

### `arch-no-circuit-breaker`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Code paths with no circuit-breaker pattern around LLM calls.
- **Why it costs:** Cascading failures during provider outages = wasted retry budget.
- **Fix:** Recommend circuit-breaker pattern.

### `arch-no-fallback-chain`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P1**
- **Detect:** Single-provider dependency with no fallback path.
- **Why it costs:** Provider outages = full feature outage; also no automatic cost optimization via cheaper alternatives.
- **Fix:** Recommend fallback configuration (point at Gateway).

### `arch-no-deduplication`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Patterns where identical requests fire in parallel (e.g., from multiple instances of same component).
- **Why it costs:** Pay N× for one answer.
- **Fix:** Recommend request coalescing.

### `arch-no-rate-limiting`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P2**
- **Detect:** User-facing LLM endpoints without rate limiting.
- **Why it costs:** Abuse vector for cost explosion.
- **Fix:** Recommend rate limit middleware.

### `arch-no-prompt-registry`
- **Tier:** 2 · **Severity:** low · **Priority:** **P3**
- **Detect:** Prompts scattered inline across files instead of centralized.
- **Why it costs:** Indirect — prevents centralized optimization.
- **Fix:** Recommend prompt registry pattern.

### `arch-no-eval-suite`
- **Tier:** 3 · **Severity:** medium · **Priority:** **P3**
- **Detect:** Production LLM features without an eval suite.
- **Why it costs:** Can't safely swap models or compress prompts because quality regression goes undetected.
- **Fix:** Recommend eval framework adoption.

---

## 9. AGENTS.md / CLAUDE.md / project config

**Theme:** The instructions file that AI agents read. Missing, bloated, or stale = wasted tokens and worse outcomes.

### `config-no-agents-md`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P0**
- **Detect:** No `AGENTS.md`, `CLAUDE.md`, `.cursor/rules`, or equivalent project guidance file in repo root.
- **Why it costs:** AI tools work without context; users compensate with verbose prompts each time. Indirect cost.
- **Fix:** Offer to generate a starter `AGENTS.md` from codebase analysis.

### `config-agents-md-too-long`
- **Tier:** 1 · **Severity:** high · **Priority:** **P0**
- **Detect:** `AGENTS.md` / `CLAUDE.md` over 4,000 tokens.
- **Why it costs:** This file is loaded into every agent context. Bloat × every call = real cost.
- **Fix:** Suggest splitting into subfolder-specific files; identify low-value sections.

### `config-agents-md-stale`
- **Tier:** 3 · **Severity:** medium · **Priority:** **P2**
- **Detect:** `AGENTS.md` references files, scripts, or commands that no longer exist in the repo.
- **Why it costs:** Agent follows wrong instructions; wasted iterations.
- **Fix:** Show specific stale references with current alternatives.

### `config-agents-md-missing-build`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** `AGENTS.md` missing build, test, or run commands.
- **Why it costs:** Agents discover them by trial and error = wasted tokens.
- **Fix:** Suggest sections to add; populate from `package.json` scripts, `Makefile`, etc.

### `config-agents-md-missing-conventions`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** `AGENTS.md` missing code style or convention guidance for a codebase with detectable strong conventions.
- **Why it costs:** Agents produce non-conforming code; users send correction prompts.
- **Fix:** Suggest convention section, optionally generated from existing code patterns.

### `config-agents-md-contains-secrets`
- **Tier:** 1 · **Severity:** critical · **Priority:** **P0**
- **Detect:** API keys, tokens, passwords, or other secrets in `AGENTS.md`.
- **Why it costs:** Critical for security, not just cost. Hard fail.
- **Fix:** Immediate removal recommendation; suggest secret management approach.

### `config-multiple-conflicting-files`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P2**
- **Detect:** `AGENTS.md` and `CLAUDE.md` (or similar) both present with conflicting instructions.
- **Why it costs:** Confused agents; both files loaded = duplication.
- **Fix:** Recommend consolidation.

### `config-agents-md-no-examples`
- **Tier:** 2 · **Severity:** low · **Priority:** **P3**
- **Detect:** `AGENTS.md` missing concrete examples of correct/incorrect patterns.
- **Why it costs:** Indirect — examples reduce ambiguity, reducing iteration.
- **Fix:** Suggest example section.

### `config-agents-md-no-mcp-docs`
- **Tier:** 1 · **Severity:** low · **Priority:** **P2**
- **Detect:** Project uses MCP servers but `AGENTS.md` doesn't document them.
- **Why it costs:** Agents under-utilize available tools; users explain manually.
- **Fix:** Generate MCP server documentation section.

---

## 10. MCP opportunities

**Theme:** Patterns in code that suggest extracting to an MCP server would reduce context bloat and improve reuse.

### `mcp-docs-stuffed-in-prompts`
- **Tier:** 2 · **Severity:** high · **Priority:** **P1**
- **Detect:** Static documentation injected into prompts repeatedly across files (detected by content similarity).
- **Why it costs:** Same docs paid for on every call.
- **Fix:** Recommend converting docs into an MCP resource server.

### `mcp-custom-filesystem-tools`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Custom filesystem-access tools (read file, list directory, etc.) implemented per project.
- **Why it costs:** Reinventing the wheel; verbose tool descriptions repeated.
- **Fix:** Point at official filesystem MCP server.

### `mcp-custom-git-tools`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Custom git-operation tools.
- **Why it costs:** Same.
- **Fix:** Point at official git MCP server.

### `mcp-hardcoded-api-client`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Hardcoded HTTP API integrations in agent tools that have community MCP servers (GitHub, Slack, Notion, Linear, etc.).
- **Why it costs:** Maintenance burden + verbose tool descriptions.
- **Fix:** Recommend specific community MCP server with link.

### `mcp-repeated-tool-impls-across-projects`
- **Tier:** 3 · **Severity:** low · **Priority:** **P3**
- **Detect:** Same tools re-implemented across multiple repos in an org.
- **Why it costs:** Effort, not direct cost — but reduces ability to centralize optimizations.
- **Fix:** Recommend extracting to internal MCP server.

### `mcp-db-queries-as-tools`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Tools that wrap database queries in custom code.
- **Why it costs:** Same — reinventing standard patterns.
- **Fix:** Point at database MCP servers (Postgres, MySQL, SQLite).

### `mcp-internal-knowledge-base`
- **Tier:** 3 · **Severity:** low · **Priority:** **P3**
- **Detect:** Large internal knowledge bases being injected into prompts.
- **Why it costs:** Token bloat across many calls.
- **Fix:** Recommend building a custom MCP server with hierarchical retrieval.

---

## 11. Cost governance & monitoring

**Theme:** Process-level issues. Detected mostly via Gateway telemetry once available, partly via repo patterns.

### `gov-no-budget-alerts`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** No budget alert configuration detected in org's Gateway settings.
- **Why it costs:** Cost overruns discovered weeks later.
- **Fix:** Recommend budget alert setup.

### `gov-mixed-environments`
- **Tier:** 1 · **Severity:** high · **Priority:** **P1**
- **Detect:** Same API key used across dev, staging, and prod (detected by Gateway tag patterns).
- **Why it costs:** Can't isolate dev waste from prod spend.
- **Fix:** Recommend per-environment keys with tags.

### `gov-no-feature-attribution`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** LLM calls without `X-TokenTrimmer-Tag` headers or equivalent attribution.
- **Why it costs:** Can't identify which features drive spend.
- **Fix:** Recommend tagging convention.

### `gov-no-anomaly-detection`
- **Tier:** 1 · **Severity:** low · **Priority:** **P3**
- **Detect:** No anomaly detection setup (Gateway feature).
- **Why it costs:** Spend spikes go unnoticed.
- **Fix:** Enable anomaly detection in dashboard.

### `gov-no-cost-diff-on-pr`
- **Tier:** 1 · **Severity:** low · **Priority:** **P2**
- **Detect:** Repo without TokenTrimmer Watch PR bot installed.
- **Why it costs:** Cost regressions ship to production undetected.
- **Fix:** Install GitHub Action (deferred to Watch product).

---

## 12. Provider-specific optimizations

**Theme:** Each provider has unique features that, if unused, leave money on the table.

### `provider-anthropic-no-batch`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** Anthropic calls in batch-processing patterns (loops over datasets) not using Message Batches API (50% discount).
- **Why it costs:** Missing 50% discount.
- **Fix:** Recommend Batches API for the workload.

### `provider-openai-no-batch`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** OpenAI calls in batch-processing patterns not using Batch API (50% discount).
- **Why it costs:** Same.
- **Fix:** Recommend Batch API.

### `provider-openai-no-predicted-outputs`
- **Tier:** 2 · **Severity:** low · **Priority:** **P2**
- **Detect:** Code-editing or text-rewriting use cases not using OpenAI Predicted Outputs.
- **Why it costs:** Misses speedup + cost discount on partial-rewrite tasks.
- **Fix:** Recommend Predicted Outputs.

### `provider-gemini-no-context-caching`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Gemini calls with large context (>32K tokens) repeated without using Gemini context caching.
- **Why it costs:** Gemini context caching offers significant discount on cached portions.
- **Fix:** Recommend Gemini context caching.

### `provider-wrong-region`
- **Tier:** 2 · **Severity:** low · **Priority:** **P3**
- **Detect:** API calls to suboptimal regions for the user's location.
- **Why it costs:** Latency, sometimes minor cost differences.
- **Fix:** Recommend region change.

### `provider-no-service-tier`
- **Tier:** 1 · **Severity:** low · **Priority:** **P3**
- **Detect:** OpenAI calls not using `service_tier: "flex"` or similar where applicable.
- **Why it costs:** Misses cost tier discounts on non-latency-sensitive workloads.
- **Fix:** Recommend appropriate service tier.

### `provider-finetune-opportunity`
- **Tier:** 3 · **Severity:** medium · **Priority:** **P3**
- **Detect:** High-volume narrow tasks (>1M calls/month detected via Gateway) that would benefit from fine-tuning.
- **Why it costs:** Fine-tuned models are 50–90% cheaper for narrow tasks.
- **Fix:** Recommend fine-tuning pipeline.

---

## 13. Local & hybrid opportunities

**Theme:** Tasks that could run locally for free, with appropriate hybrid routing.

### `local-classification-eligible`
- **Tier:** 2 · **Severity:** high · **Priority:** **P1**
- **Detect:** Classification tasks at high volume that small local models handle well.
- **Why it costs:** Paying cloud rates for work a 7B local model does well.
- **Fix:** Recommend hybrid routing via Gateway with local model.

### `local-embedding-eligible`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** High-volume embedding generation using cloud APIs.
- **Why it costs:** BGE-small or similar local model can generate embeddings at near-zero cost.
- **Fix:** Recommend local embedding model.

### `local-pii-eligible`
- **Tier:** 3 · **Severity:** high · **Priority:** **P2**
- **Detect:** PII-handling patterns sending sensitive data to cloud providers.
- **Why it costs:** Privacy risk + cloud cost.
- **Fix:** Recommend local model for PII tasks.

### `local-first-pass-eligible`
- **Tier:** 3 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Chatbot patterns where simple intents could be handled locally with cloud fallback.
- **Why it costs:** Easy queries paid at cloud rates.
- **Fix:** Recommend local-first-pass routing with confidence cascade.

---

## 14. Library-specific anti-patterns

**Theme:** Common waste patterns in popular LLM libraries.

### `lib-langchain-verbose-default-prompts`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** LangChain agents using default ReAct or chat prompts (which are verbose).
- **Why it costs:** Default prompts can be 1,000+ tokens of boilerplate.
- **Fix:** Show custom prompt template alternative.

### `lib-langchain-chain-redundancy`
- **Tier:** 2 · **Severity:** medium · **Priority:** **P2**
- **Detect:** LangChain LCEL chains making multiple LLM calls where one would do.
- **Why it costs:** Each chain step = LLM call.
- **Fix:** Suggest consolidation.

### `lib-llamaindex-no-caching`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P2**
- **Detect:** LlamaIndex query engines without query caching enabled.
- **Why it costs:** Repeated identical queries hit the LLM.
- **Fix:** Recommend caching middleware.

### `lib-vercel-ai-no-middleware`
- **Tier:** 1 · **Severity:** low · **Priority:** **P2**
- **Detect:** Vercel AI SDK usage without caching middleware.
- **Why it costs:** No exact-match cache by default.
- **Fix:** Recommend middleware setup or TokenTrimmer Gateway.

### `lib-openai-sdk-no-streaming`
- **Tier:** 1 · **Severity:** low · **Priority:** **P3**
- **Detect:** User-facing OpenAI SDK calls without streaming.
- **Why it costs:** UX degradation.
- **Fix:** Recommend streaming pattern.

### `lib-anthropic-sdk-no-cache-control`
- **Tier:** 1 · **Severity:** high · **Priority:** **P0**
- **Detect:** Anthropic SDK calls with long system prompts missing `cache_control`.
- **Why it costs:** 90% input cost discount left on the floor.
- **Fix:** Show diff with `cache_control` block.
- (Note: this overlaps with `cache-anthropic-prompt-cache-missing` — kept separate because this one is library-specific and may catch edge cases.)

### `lib-pydantic-ai-no-validator-cache`
- **Tier:** 2 · **Severity:** low · **Priority:** **P3**
- **Detect:** Pydantic-AI usage where validators trigger LLM re-calls on parse failure without caching.
- **Why it costs:** Re-validation = re-LLM-call.
- **Fix:** Recommend validator caching.

---

## 15. Hidden cost sources

**Theme:** Costs that don't show up in obvious LLM call patterns.

### `hidden-eval-in-production`
- **Tier:** 2 · **Severity:** high · **Priority:** **P1**
- **Detect:** Evaluator LLM calls running on every production request (e.g., LLM-as-judge in critical path).
- **Why it costs:** Doubles or triples per-request cost.
- **Fix:** Recommend sampling-based evaluation (e.g., 1% of traffic) instead of 100%.

### `hidden-reranker-cost`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P2**
- **Detect:** Reranker calls (Cohere Rerank, etc.) not tracked as LLM cost.
- **Why it costs:** Reranker spend often surprises teams.
- **Fix:** Surface reranker cost separately in dashboards.

### `hidden-debug-logging`
- **Tier:** 1 · **Severity:** medium · **Priority:** **P1**
- **Detect:** Debug code paths that send prompts to additional LLM calls for inspection.
- **Why it costs:** Doubles cost when enabled.
- **Fix:** Recommend gating debug calls behind environment flag.

### `hidden-telemetry-inflation`
- **Tier:** 1 · **Severity:** low · **Priority:** **P3**
- **Detect:** Telemetry that includes full prompts/responses repeatedly.
- **Why it costs:** Storage cost, sometimes LLM cost if telemetry pipeline analyzes.
- **Fix:** Recommend trimming telemetry payloads.

### `hidden-tool-init-cost`
- **Tier:** 2 · **Severity:** low · **Priority:** **P3**
- **Detect:** Tools that make LLM calls during initialization on every agent instantiation.
- **Why it costs:** Each agent spawn pays init cost.
- **Fix:** Recommend caching init results.

---

## v1 launch P0 rules — the shortlist (15)

These ship with the v1 binary. Conservative selection, high confidence, broad applicability.

| Rule | Family | Tier | Severity |
|---|---|---|---|
| `cache-anthropic-prompt-cache-missing` | Caching | 1 | critical |
| `cache-openai-prompt-cache-eligible` | Caching | 1 | high |
| `lib-anthropic-sdk-no-cache-control` | Library | 1 | high |
| `model-flagship-for-classification` | Model selection | 2 | high |
| `model-flagship-for-extraction` | Model selection | 2 | high |
| `model-deprecated` | Model selection | 1 | medium |
| `prompt-bloated-system` | Prompt | 2 | high |
| `prompt-verbose-few-shot` | Prompt | 2 | medium |
| `prompt-no-output-constraint` | Prompt | 1 | medium |
| `output-no-max-tokens` | Output | 1 | medium |
| `conversation-unbounded-history` | Conversation | 2 | high |
| `agent-no-termination-condition` | Agent | 2 | critical |
| `config-no-agents-md` | Config | 1 | medium |
| `config-agents-md-too-long` | Config | 1 | high |
| `config-agents-md-contains-secrets` | Config | 1 | critical |

Rationale: every P0 rule is detectable mechanically (Tier 1) or via a single small-model call (Tier 2). False-positive risk is minimized. Each has a clear, defensible fix.

---

## Roadmap summary

- **v1 launch:** 15 P0 rules
- **+60 days (P1):** ~40 additional rules — covers most of Anthropic/OpenAI provider optimizations, agent patterns, conversation management
- **v2 (+6 months, P2):** ~40 additional rules — MCP detection, RAG optimization, library-specific deep checks
- **v3+ (P3):** ~25 long-term rules — architectural refactor recommendations, eval-suite generation, cross-repo learning

Total target catalog at v3: ~120 rules.

---

## Open questions

1. **How conservative on confidence thresholds?** Recommend 0.85 confidence minimum for any rule that auto-creates a PR diff; 0.7 for warnings only. Lower threshold = more findings = more value but also more false positives = trust damage.

2. **Should rules be authored externally?** Recommend yes — community rules repo (`tokentrimmer/rules-contrib`) with PR review. Lets the catalog grow without core team writing every rule.

3. **Cost ceiling per scan?** Recommend hard cap of $0.10 per scan in CI mode (per repo, per PR), $1.00 for deep scans (weekly), configurable per tier.

4. **How to handle rule deprecation?** Recommend rule versioning; deprecated rules emit warnings for one minor version then are removed.

5. **Multi-language same-rule deduplication?** Each rule should specify supported languages; same logical issue across Python and TypeScript can be one rule with two parsers.
