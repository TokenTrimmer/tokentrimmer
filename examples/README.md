# TokenTrimmer SDK examples

Runnable snippets for the Python, TypeScript, and Rust clients. Each needs a
TokenTrimmer API key (or a self-hosted gateway). Use a sandbox `tt_test_*` key
to exercise the wire path without real provider calls.

## Python
```bash
pip install tokentrimmer
export TOKENTRIMMER_API_KEY=tt_...
python examples/python/cost_attribution.py
```

## TypeScript
```bash
npm install @tokentrimmer/client
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
