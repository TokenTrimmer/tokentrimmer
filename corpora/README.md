# Inspect FP-rate corpus

A small corpus of **realistic, idiomatic LLM-SDK usage** used to measure the
false-positive rate of the Inspect rules on real-world-style code (the w24 FP
gate: FP rate must stay < 5%).

## Provenance & licensing

These samples are **authored in-repo** as faithful, idiomatic representations of
common patterns from the OpenAI Python/Node SDKs, the Anthropic SDK, the Vercel
AI SDK, and LangChain — the same surfaces the rules target. They are NOT copied
from upstream repositories: vendoring pinned upstream files would require
fetching them with their licenses + attribution, which is a follow-up that needs
network access. Authoring representative samples keeps the corpus license-clean
(it ships under this repo's licence) while still exercising the rules against
correct, production-shaped code.

Each sample reflects **best practice** (explicit `max_tokens`, current models,
`cache_control` where it pays off, `n`=1, bounded agent loops, static prompt
prefixes), so any finding the rules emit here is a presumed **false positive**.

## Measuring

```
cargo build -p tt-cli --bin tt
./scripts/measure-fp-rate.sh --corpus corpora/samples
```

The latest recorded result is in `FP_REPORT.md`.
