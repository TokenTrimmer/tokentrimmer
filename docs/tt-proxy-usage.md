# tt proxy

Local OpenAI/Anthropic-compatible listener that routes through the hosted
TokenTrimmer Gateway and writes per-session cost rollups.

## Quick start

```bash
tt proxy --mode gateway --tt-api-key tt_live_...
export ANTHROPIC_BASE_URL=http://localhost:31415
# or for Codex: export OPENAI_BASE_URL=http://localhost:31415
```

## Modes

- `gateway` (default) — forward to hosted TT Gateway. Requires `--tt-api-key` or `TT_API_KEY`.
- `bypass` — forward directly to the upstream provider. Logging only, no features.
- `hybrid` — gateway for `/v1/chat/completions`, bypass for everything else.

## Session log

JSONL appended at `~/.tokentrimmer/sessions/YYYY-MM-DD.jsonl`. One line per request.
On Ctrl-C the proxy prints a session summary (disable with `--no-tui`).
