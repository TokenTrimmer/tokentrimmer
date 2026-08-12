# tt proxy

Local OpenAI/Anthropic-compatible listener that routes both OpenAI-wire and
Anthropic-wire traffic through the hosted TokenTrimmer Gateway and writes
per-session cost rollups. The Gateway exposes an Anthropic-native `/v1/messages`
ingress that runs the same routing/cache/failover pipeline as
`/v1/chat/completions`, so Anthropic-wire clients (Claude Code, Cursor) get the
same optimization as OpenAI-wire clients in `gateway`/`hybrid` mode.

> **`tt proxy` is not self-hosting.** It is a *local egress shim* — a listener on
> port 31415. To **self-host the Gateway itself**, run `tt gateway` (the
> OpenAI-compatible server, port 8080) — see the self-host section in
> `GETTING_STARTED.md` / `README.md`. `hybrid` may point `tt proxy` only at a
> loopback self-hosted Gateway, such as `--tt-api-base http://127.0.0.1:8080`.

## Quick start

```bash
tt proxy --mode gateway --tt-api-key tt_live_...
export ANTHROPIC_BASE_URL=http://localhost:31415
# or for an OpenAI Chat Completions client:
export OPENAI_BASE_URL=http://localhost:31415
```

Current Codex CLI requires streamed `/v1/responses`; this listener does not expose that endpoint. Run Codex directly rather than pointing it at `tt proxy`.

## Modes

- `gateway` (default) — all endpoints (`/v1/chat/completions`, `/v1/messages`, `/v1/models`) forward to the hosted TT Gateway. The proxy removes client provider credentials and injects only your TokenTrimmer key. Requires `--tt-api-key` or `TT_API_KEY`.
- `bypass` — forward directly to the upstream provider (OpenAI for OpenAI-wire, Anthropic for `/v1/messages`) with the client's provider credential. Logging only, no TokenTrimmer features.
- `hybrid` — all endpoints forward only to an explicitly configured loopback self-hosted Gateway. The proxy preserves the client's provider credential and never injects a TokenTrimmer key. Remote targets are rejected.

In `gateway` and `hybrid` mode `/v1/messages` routes through the selected Gateway; only `bypass` forwards it directly to the Anthropic upstream.

## Session log

JSONL appended at `~/.tokentrimmer/sessions/YYYY-MM-DD.jsonl`. One line per request.
On Ctrl-C the proxy prints a session summary (disable with `--no-tui`).
