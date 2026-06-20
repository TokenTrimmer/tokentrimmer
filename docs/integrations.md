# Integration guides — n8n, LangChain, LangGraph, Dify

> **Hosted gateway launching soon** *(as of 2026-06-11)* — `https://api.tokentrimmer.com/v1` is not live yet. Self-host with Docker today and use `http://localhost:8080/v1`; every snippet below works unchanged once you swap the base URL. See [`GETTING_STARTED.md`](../GETTING_STARTED.md) for the one-`docker run` quickstart.

Every builder below already speaks the OpenAI wire format. Point it at the TokenTrimmer Gateway instead of the provider and every call comes back with `x-tokentrimmer-cost-usd` / `x-tokentrimmer-baseline-cost-usd` / `x-tokentrimmer-saved-usd` response headers — proof of what the call cost and what routing/caching saved — while the `X-TokenTrimmer-Tag` request header gives per-feature dashboard attribution (hosted). Full header semantics: [Gateway API reference §6](04-gateway-api-reference.md).

## n8n — community node (preferred)

The [`n8n-nodes-tokentrimmer`](../n8n-nodes-tokentrimmer/README.md) package ships a **TokenTrimmer API** credential (API key + base URL) and a **TokenTrimmer Chat** node that calls `POST /v1/chat/completions` and attaches a `costInfo` object — parsed from the `x-tokentrimmer-*` response headers — to every execution's output:

```json
{ "costUsd": 0.0034, "baselineCostUsd": 0.0218, "savedUsd": 0.0184, "savingsPct": 84.4, "modelUsed": "claude-3-5-haiku-20241022", "cache": "hit-l1" }
```

Saved USD per workflow run, visible in the output panel and usable by downstream nodes.

> **Not yet on npm** — the package publishes at launch. Until then, install from source: see the [package README](../n8n-nodes-tokentrimmer/README.md) for the `~/.n8n/nodes` install path.

**What you get:** every n8n execution carries `costInfo.savedUsd` (plus cost, baseline, model used, cache status).

## n8n — vanilla HTTP Request node (works today, no install)

Configure an **HTTP Request** node:

- **Method:** `POST`, **URL:** `http://localhost:8080/v1/chat/completions`
- **Authentication:** Generic Credential Type → Header Auth → name `Authorization`, value `Bearer tt_live_...` (hosted) or your provider key (self-host pass-through)
- **Send Headers** → `X-TokenTrimmer-Tag`: `feature=my-workflow`
- **Send Body** → JSON:

```json
{"model": "claude-sonnet-4-6", "messages": [{"role": "user", "content": "{{ $json.chatInput }}"}]}
```

**What you get:** enable the node's "Include Response Headers and Status" option and `headers['x-tokentrimmer-saved-usd']` (and the rest of the `x-tokentrimmer-*` family) is available to downstream nodes.

## LangChain (Python)

```python
from langchain_openai import ChatOpenAI

llm = ChatOpenAI(
    model="claude-sonnet-4-6",
    base_url="http://localhost:8080/v1",   # hosted: https://api.tokentrimmer.com/v1
    api_key="tt_live_...",                  # self-host pass-through: your provider key
    default_headers={"X-TokenTrimmer-Tag": "feature=support-bot"},
)
print(llm.invoke("Hello").content)
```

**What you get:** every chain/agent call is routed and cached by the gateway, with cost headers on each response and `feature=support-bot` attribution on the hosted dashboard.

## LangChain (JS/TS)

```ts
import { ChatOpenAI } from '@langchain/openai';

const llm = new ChatOpenAI({
  model: 'claude-sonnet-4-6',
  apiKey: 'tt_live_...',
  configuration: {
    // `configuration` is the underlying OpenAI client config.
    baseURL: 'http://localhost:8080/v1',
    defaultHeaders: { 'X-TokenTrimmer-Tag': 'feature=support-bot' },
  },
});
console.log((await llm.invoke('Hello')).content);
```

**What you get:** same as Python — gateway routing, caching, cost headers, and tag attribution on every call.

## LangGraph

LangGraph inherits the LangChain client configuration — build the `llm` exactly as above and pass it into your graph:

```python
from langgraph.prebuilt import create_react_agent

agent = create_react_agent(llm, tools)  # the gateway-pointed ChatOpenAI from above
```

**What you get:** every agent step (including tool-use loops) is routed, costed, and tagged — no per-node configuration.

## Dify

Settings → **Model Provider** → **OpenAI-API-compatible** → add a model:

- **Model Name:** e.g. `claude-sonnet-4-6`
- **API Key:** `tt_live_...` (hosted) or your provider key (self-host pass-through)
- **API endpoint URL:** `http://localhost:8080/v1`

Caveat: Dify's provider UI cannot set custom request headers, so per-request `X-TokenTrimmer-Tag` attribution isn't available. Routing and cache savings are still computed and recorded gateway-side on every call.

**What you get:** all Dify app calls flow through the gateway and benefit from routing/caching, with cost headers on each response.

## Agent stacks (MCP)

For Claude Code / Cursor / any MCP client, `tt mcp` exposes the gateway's cost, plan, and routing tools (read-only). Mutating write tools (`add_route`, `apply_plan`) ship in the MCP server behind an off-by-default write gate; pass `tt mcp --allow-write` to enable them (requires `DATABASE_URL` so the operator key can be verified and org-bound at boot — it refuses to start otherwise). See [`tt-mcp-usage.md`](tt-mcp-usage.md).
