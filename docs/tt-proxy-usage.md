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

## Mode and credential contract

Run `tt proxy --help` for the authoritative mode reference. Clap renders each
mode directly from the same `ModeContract` used by startup validation, upstream
selection, and request credential handling; prose copies are intentionally not
maintained here.

## Session log

JSONL appended at `~/.tokentrimmer/sessions/YYYY-MM-DD.jsonl`. One line per request.
On Ctrl-C the proxy prints a session summary (disable with `--no-tui`).
