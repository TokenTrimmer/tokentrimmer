# TokenTrimmer SDK examples

Runnable snippets for the Python, TypeScript, and Rust clients. Each needs a
TokenTrimmer API key (or a self-hosted gateway). Use a sandbox `tt_test_*` key
to exercise the wire path without real provider calls.

> **Not yet on PyPI/npm** — published packages land at launch. The snippets
> below use the working git/local installs until then.

## Python
```bash
pip install "git+https://github.com/tokentrimmer/tokentrimmer.git#subdirectory=sdk-python"
export TOKENTRIMMER_API_KEY=tt_...
python examples/python/cost_attribution.py
```

## TypeScript
```bash
# npm cannot install a git subdirectory directly — build the SDK from a clone:
git clone https://github.com/tokentrimmer/tokentrimmer.git
(cd tokentrimmer/sdk-typescript && npm install && npm run build)
npm install ./tokentrimmer/sdk-typescript
export TOKENTRIMMER_API_KEY=tt_...
npx tsx examples/typescript/cost-attribution.ts
```

## Rust
```bash
export TOKENTRIMMER_API_KEY=tt_...
cargo run -p tt-client --example cost_attribution
```

`.tt` (Python/TS) / `outcome.cost` (Rust) carries the gateway's cost metadata:
`cost_usd`, `baseline_cost_usd`, `saved_usd`, `cache`, `provider`, `model_used`,
`trace_id`. On a streaming call the SDKs return the raw stream and do NOT attach
`.tt` — read terminal `usage` off the final chunk.
