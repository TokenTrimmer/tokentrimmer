# Cost Preview API

`POST /v1/preview` — synchronous, no LLM calls, no Postgres lookups.

## Request

```json
{
  "model": "claude-haiku-4-5",
  "messages": [{"role": "user", "content": "..."}],
  "max_tokens": 1024
}
```

## Response

```json
{
  "current": {
    "model": "claude-haiku-4-5",
    "provider": "anthropic",
    "input_tokens_estimated": 12,
    "output_tokens_estimated": 100,
    "cost_usd": 0.000023,
    "estimation_confidence": "high"
  },
  "cache_projections": { ... },
  "route_suggestions": [ ... ],
  "warnings": [],
  "trace_id": "..."
}
```

See `docs/superpowers/specs/2026-05-28-trackC-cost-preview-api-design.md` for the design rationale.
