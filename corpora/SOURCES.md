# Vendored corpus sources

The FP corpus has two tiers:

1. **`corpora/samples/`** — authored-in-repo idiomatic samples (always present,
   license-clean, runnable in CI today). See `corpora/README.md`.
2. **`corpora/vendor/<name>/`** — *real* permissively-licensed OSS slices,
   fetched reproducibly with `scripts/vendor-corpora.sh`. This tier **is
   populated and committed** — see "Vendored slices (committed)" below.

## Why generic + pinned

Upstream repos restructure. Rather than hardcode paths that bitrot, the vendor
script takes the repo, a **pinned commit SHA**, and a glob. `vendor-corpora.sh`
**requires a full 40-hex SHA** (it rejects branch names / `HEAD`) and asserts the
checked-out commit equals it, so the fetch is reproducible. The script records
the pinned commit in `corpora/vendor/<name>/.source` and copies the upstream
`LICENSE` so provenance + licence travel with the code.

## Vendored slices (committed)

Each was vendored at the pinned commit below; the upstream `LICENSE` is captured
alongside in the slice directory.

| name | pinned commit | glob |
|---|---|---|
| `openai-python`        | `09ece2ee0aa73077d04ac3669d101120076b52fb` | `examples/*.py` |
| `openai-cookbook`      | `42bc4ed33a2d1ee308c1f614c073f1b5f760061b` | `examples/*.py` |
| `anthropic-sdk-python` | `ddd43b7ceb79f433dfafe488d95f48c22801186b` | `examples/*.py` |
| `vercel-ai`            | `ab6d66482d31afe15f4973a51c5f7cfa09c92ea6` | `examples/*.ts` |

`langchain` (below) is curated but **not yet vendored**.

## Curated sources

Permissively licensed (MIT / Apache-2.0) LLM-SDK repos with idiomatic example
code. **Verify the licence** (the script vendors it) before committing.

| name | repo | expected licence | suggested glob |
|---|---|---|---|
| `openai-python`        | `https://github.com/openai/openai-python`            | Apache-2.0 | `examples/*.py` |
| `openai-cookbook`      | `https://github.com/openai/openai-cookbook`          | MIT        | `examples/*.py` |
| `anthropic-sdk-python` | `https://github.com/anthropics/anthropic-sdk-python` | MIT        | `examples/*.py` |
| `vercel-ai`            | `https://github.com/vercel/ai`                       | Apache-2.0 | `examples/**/*.ts` |
| `langchain`            | `https://github.com/langchain-ai/langchain`          | MIT        | `cookbook/*.py` |

## Workflow

```bash
cargo build -p tt-cli --bin tt

# Pin a SHA from the repo, then vendor a slice:
./scripts/vendor-corpora.sh openai-cookbook \
    https://github.com/openai/openai-cookbook <pinned-sha> 'examples/*.py' 8

# Measure FP rate on the real-code corpus:
./scripts/measure-fp-rate.sh --corpus corpora/vendor

# Review licences, then commit the slices:
git add corpora/vendor && git commit -m "chore(corpora): vendor pinned OSS samples"
```

The 5% FP gate (`measure-fp-rate.sh --corpus`) applies equally to
`corpora/samples` and `corpora/vendor`; run it over whichever corpus you have.
