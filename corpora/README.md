# Inspect FP-rate corpus

A small corpus of **realistic, idiomatic LLM-SDK usage** used to measure the
false-positive rate of the Inspect rules on real-world-style code (the w24 FP
gate: FP rate must stay < 5%).

## Two tiers

The corpus has two tiers (see `SOURCES.md` for the full provenance table):

1. **`corpora/samples/`** — **authored in-repo** as faithful, idiomatic
   representations of common patterns from the OpenAI Python/Node SDKs, the
   Anthropic SDK, the Vercel AI SDK, and LangChain. NOT copied from upstream, so
   they ship license-clean under this repo's licence.
2. **`corpora/vendor/<name>/`** — **real permissively-licensed upstream slices**,
   vendored reproducibly via `scripts/vendor-corpora.sh` at a pinned commit SHA,
   with the upstream `LICENSE` and a `.source` provenance file. This tier **is
   populated and committed**: openai-python, openai-cookbook, anthropic-sdk-python
   (examples), and vercel-ai (examples). See `SOURCES.md` for the pinned commits.

Each sample (authored or vendored) reflects **best practice** (explicit
`max_tokens`, current models, `cache_control` where it pays off, `n`=1, bounded
agent loops, static prompt prefixes), so any finding the rules emit on it is a
presumed **false positive** (the w24 FP gate: FP rate must stay < 5%).

## Measuring

```
cargo build -p tt-cli --bin tt
./scripts/measure-fp-rate.sh --corpus corpora/samples
./scripts/measure-fp-rate.sh --corpus corpora/vendor   # the vendored tier
```

The latest recorded result is in `FP_REPORT.md`.
