# tt proxy

Local OpenAI/Anthropic-compatible listener that routes both OpenAI-wire and
Anthropic-wire traffic through the hosted TokenTrimmer Gateway and writes
per-session cost rollups. The Gateway exposes an Anthropic-native `/v1/messages`
ingress that runs the same routing/cache/failover pipeline as
`/v1/chat/completions`, so Anthropic-wire clients (Claude Code, Cursor) get the
same optimization as OpenAI-wire clients in `gateway`/`hybrid` mode.

> **`tt proxy` is not self-hosting.** It is a *local egress shim* — a listener on
> port 31415 that forwards your app's OpenAI/Anthropic-wire traffic to a remote
> Gateway. To **self-host the Gateway itself**, run `tt gateway` (the
> OpenAI-compatible server, port 8080) — see the self-host section in
> `GETTING_STARTED.md` / `README.md`. Point `tt proxy` at your self-hosted
> Gateway with `--tt-api-base http://localhost:8080`.

## Quick start

```bash
tt proxy --mode gateway --tt-api-key tt_live_...
export ANTHROPIC_BASE_URL=http://localhost:31415
# or for Codex: export OPENAI_BASE_URL=http://localhost:31415
```

## Modes

- `gateway` (default) — all endpoints (`/v1/chat/completions`, `/v1/messages`, `/v1/models`) forward to the hosted TT Gateway with your TokenTrimmer key injected. Requires `--tt-api-key` or `TT_API_KEY`.
- `bypass` — forward directly to the upstream provider (OpenAI for OpenAI-wire, Anthropic for `/v1/messages`). Logging only, no features.
- `hybrid` — all endpoints to the gateway, but your client's own credentials pass through (no TokenTrimmer key injection).

In `gateway` and `hybrid` mode `/v1/messages` routes through the Gateway (getting routing/caching/failover); only `bypass` forwards it directly to the Anthropic upstream.

## Session log

JSONL appended at `~/.tokentrimmer/sessions/YYYY-MM-DD.jsonl`. One line per request.
On Ctrl-C the proxy prints a session summary (disable with `--no-tui`).
