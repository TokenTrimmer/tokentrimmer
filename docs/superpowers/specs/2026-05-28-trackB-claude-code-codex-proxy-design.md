# Track B — Claude Code / Codex Proxy (`tt proxy`)

**Status:** Draft 1
**Track:** B of six-track expansion
**Date:** 2026-05-28
**Depends on:** Track C (cost preview endpoint) for inline cost annotations
**Consumed by:** end users (developers running Claude Code / Codex / Cursor locally)

---

## 1. Problem

Developers running Claude Code, Codex, or Cursor against `api.anthropic.com` / `api.openai.com` have:
1. No visibility into per-session cost while it accrues.
2. No automatic caching across sessions — every repeat query hits the upstream.
3. No routing — every request goes to the model the IDE chose, even when a cheaper one would suffice.

The Anthropic SDK respects `ANTHROPIC_BASE_URL`; OpenAI SDK respects `OPENAI_BASE_URL`. If a developer sets either to a local `tt proxy` listener, we transparently get traffic to optimize and observe.

`tt proxy` runs a local HTTP listener (default port 31415) that:
- Speaks both the OpenAI and Anthropic native APIs.
- Forwards to either (a) the hosted TokenTrimmer Gateway (default) or (b) the upstream provider directly (`--mode bypass`).
- Returns identical bytes to the IDE (no schema rewrites).
- Injects `X-TT-*` cost-preview headers on each response (uses Track C).
- Writes a session-level cost rollup to `~/.tokentrimmer/sessions/<isodate>.jsonl`.
- Prints a TUI-style banner on Ctrl-C: "This session: $0.42 → would have been $0.71 unproxied."

## 2. Goals

1. Zero-config for the user: `tt proxy` runs; user `export ANTHROPIC_BASE_URL=http://localhost:31415`; everything keeps working.
2. Transparency: the IDE sees exactly the bytes it would have seen without us.
3. Observability: every request → session log, even when forwarded in bypass mode.
4. Cost discipline: inline preview cost (Track C output) shown in the proxy's terminal UI.
5. Trust: if the proxy crashes, the IDE gets a `502` with `X-TT-Proxy-Down: true` so it can retry directly.

## 3. Non-goals

- Replacing the hosted Gateway. The proxy IS a thin client that talks to it.
- Rewriting prompts. The proxy is a transport, not an optimizer. (Optimization happens in the Gateway / Plan engine.)
- IDE-specific integrations (Cursor extensions, Claude Code plugins). Those are Track A.
- Windows-native binary. v1 is mac+linux; Windows users use WSL.

## 4. Architecture

```
crates/cli/
└── src/
    ├── main.rs                          [modified — register Proxy subcommand]
    └── proxy/
        ├── mod.rs                       [orchestrator + CLI args]
        ├── listener.rs                  [Axum server on port 31415]
        ├── routes/
        │   ├── anthropic.rs             [POST /v1/messages — Anthropic native]
        │   ├── openai.rs                [POST /v1/chat/completions — OpenAI native]
        │   └── models.rs                [GET /v1/models — both shapes]
        ├── forward.rs                   [reqwest streaming forward to upstream]
        ├── session.rs                   [session lifecycle, rollup, on-exit banner]
        ├── tui.rs                       [optional crossterm banner; --no-tui to disable]
        └── config.rs                    [~/.tokentrimmer/proxy.toml]
```

## 5. CLI surface

```
tt proxy [OPTIONS]

  Run a local OpenAI/Anthropic-compatible proxy on port 31415.

OPTIONS:
  --port <PORT>                Listener port. Default 31415.
  --bind <ADDR>                Bind address. Default 127.0.0.1.
  --mode <MODE>                gateway (default) | bypass | hybrid
                               gateway: forward to hosted TT Gateway (full features)
                               bypass:  forward to upstream provider directly (zero feature, only logging)
                               hybrid:  gateway for /v1/chat/completions, bypass for everything else
  --tt-api-key <KEY>           API key for the hosted Gateway. Falls back to TT_API_KEY env.
                               Required for --mode gateway. Ignored otherwise.
  --upstream-key <KEY>         Provider API key for --mode bypass. Falls back to ANTHROPIC_API_KEY /
                               OPENAI_API_KEY env per route hit.
  --session-log <DIR>          Where to write session JSONL. Default ~/.tokentrimmer/sessions/.
  --no-tui                     Disable the on-exit banner.
  --no-preview                 Skip the Track C preview header injection (saves one extra request).
  -h, --help                   Print help.
```

### 5.1 Setup walkthrough printed on `tt proxy --setup-help`

```
1. Start the proxy: tt proxy --mode gateway --tt-api-key tt_live_...
2. Point Claude Code at it:
     export ANTHROPIC_BASE_URL=http://localhost:31415
3. Or point Codex at it:
     export OPENAI_BASE_URL=http://localhost:31415
4. Use Claude Code / Codex / Cursor as normal. The proxy logs every request.
5. On exit (Ctrl-C), the proxy prints your session cost + what you saved.
```

## 6. Request flow

```
IDE
  ├─ POST localhost:31415/v1/messages
  ▼
tt proxy [routes/anthropic.rs]
  ├─ parse request body (validate shape, do not modify)
  ├─ if --no-preview is off:
  │    fire-and-forget POST /v1/preview to TT Gateway → store cost estimate
  ├─ forward original body to:
  │    - --mode gateway: https://tokentrimmer.fly.dev/v1/messages
  │    - --mode bypass:  https://api.anthropic.com/v1/messages
  ├─ stream response back to IDE byte-for-byte
  ├─ inject response headers:
  │    X-TT-Preview-Cost-Usd: <from cost estimate>
  │    X-TT-Actual-Cost-Usd: <from gateway header, when gateway mode>
  │    X-TT-Suggested-Route: <from preview if applicable>
  ├─ append request summary to session log
  └─ update in-memory session totals
```

## 7. Session log + rollup

Each request appends one JSONL line to `~/.tokentrimmer/sessions/<YYYY-MM-DD>.jsonl`:

```json
{
  "timestamp": "2026-05-28T15:32:11Z",
  "mode": "gateway",
  "route": "POST /v1/messages",
  "model": "claude-sonnet-4-6",
  "input_tokens": 47,
  "output_tokens": 12,
  "cost_usd": 0.000189,
  "preview_cost_usd": 0.000189,
  "cache_layer": "miss",
  "suggested_route": "swap-to-haiku-4-5",
  "suggested_savings_usd": 0.000166,
  "trace_id": "01J2Y..."
}
```

On Ctrl-C (TUI mode):

```
┌─ tokentrimmer session summary ─────────────────────┐
│  Requests:           42                            │
│  Total cost:         $0.42                         │
│  Cached (L1+L2):     8 (19%)                       │
│  Cache savings:      $0.08                         │
│  Suggested savings:  $0.18  (if all routes apply)  │
│  Net potential:      $0.16   ← what you'd save     │
│                                                    │
│  Session log: ~/.tokentrimmer/sessions/2026-05-28.jsonl
└────────────────────────────────────────────────────┘
```

## 8. Failure modes

| Failure | Behavior |
|---|---|
| Hosted Gateway 5xx | Proxy returns 502 with `X-TT-Upstream-Status: 5xx`. IDE sees a normal upstream error. |
| Hosted Gateway timeout (10s default) | Same. Add `--gateway-timeout-ms` to tune. |
| `tt_api_key` invalid | Refuse to start. Print error. |
| `tt proxy` crashes | systemd unit (linux) / launchd (mac) auto-restart suggested in --setup-help; OR user restarts manually. |
| `~/.tokentrimmer/` not writable | Print warning, continue without session log. |

## 9. Testing

| Layer | Tests |
|---|---|
| Unit (routes/anthropic) | Given canned request → forwards correct shape; preserves headers. |
| Unit (routes/openai) | Same. |
| Unit (session) | Append + rollup math correct given fixture lines. |
| Integration | Run `tt proxy` against `httpmock` Gateway → curl the proxy → assert response identical, headers added, session line written. |
| Integration (bypass) | Same but with `httpmock` Anthropic upstream. |
| Smoke (real) | Wire to `tokentrimmer.fly.dev` with a real `tt_test_*` key → assert dev-machine cost log accumulates as expected. |

## 10. Rollout

1. Day 0: ship `--mode gateway` + Anthropic native + OpenAI native. Session log + Ctrl-C banner.
2. Day 7: add `--mode bypass` for users without a hosted account.
3. Day 14: add `--mode hybrid`.
4. Day 30: add launchd / systemd unit files in `scripts/install-proxy.sh`.

## 11. References

- `ANTHROPIC_BASE_URL` env handling: Anthropic SDK source
- `OPENAI_BASE_URL` env handling: OpenAI SDK source
- Existing forward logic: `crates/core/src/routes/chat.rs`
- Existing pricing: `crates/providers/<name>/src/pricing.rs`
