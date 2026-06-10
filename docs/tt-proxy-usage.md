# tt proxy

Local OpenAI/Anthropic-compatible listener that routes OpenAI-wire traffic
through the hosted TokenTrimmer Gateway and writes per-session cost rollups.
Note: Anthropic-wire requests (`/v1/messages`) forward directly to the
Anthropic upstream in every mode, with your client's own credentials passed
through — the Gateway has no Anthropic ingress yet.

## Quick start

```bash
tt proxy --mode gateway --tt-api-key tt_live_...
export ANTHROPIC_BASE_URL=http://localhost:31415
# or for Codex: export OPENAI_BASE_URL=http://localhost:31415
```

## Modes

- `gateway` (default) — OpenAI-wire endpoints forward to the hosted TT Gateway with your TokenTrimmer key injected. Requires `--tt-api-key` or `TT_API_KEY`.
- `bypass` — forward directly to the upstream provider. Logging only, no features.
- `hybrid` — OpenAI-wire endpoints to the gateway, but your client's own credentials pass through (no TokenTrimmer key injection).

In all three modes `/v1/messages` goes directly to the Anthropic upstream.

## Session log

JSONL appended at `~/.tokentrimmer/sessions/YYYY-MM-DD.jsonl`. One line per request.
On Ctrl-C the proxy prints a session summary (disable with `--no-tui`).
