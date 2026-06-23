# Deep Research Panel — Phase 6 (transcoder rendering) Design Spec

> Status: IMPLEMENTED (Phase 6 — branch feat/panel-phase6-transcoders). Date: 2026-06-23. Repo: public. Branch: `feat/panel-phase6-transcoders`.
> Builds on Phases 1–5. Master spec: `2026-06-21-deep-research-panel-design.md` (roadmap row 6: "Render panel + per-leg attribution on `/v1/messages` + `/v1/responses`; fix `/v1/responses` `tt_extras` passthrough; verify `/v1/messages`").

## 1. Goal

The panel **already runs** on `/v1/messages` (Anthropic Messages API) and `/v1/responses` (OpenAI Responses API): both transcoders translate the inbound request to a `ChatCompletionRequest`, call the shared `chat::handler`, and the `X-TokenTrimmer-Panel` header (read from HTTP headers in `prepare`) triggers the panel exactly as on `/v1/chat/completions`. Two things are missing on these endpoints:
1. The **`tokentrimmer.panel` per-leg attribution object is dropped** — both transcoders re-parse the chat response body as `ChatCompletionResponse` (discarding the top-level `tokentrimmer` key the chat handler grafted in) and serialize a fresh target-shape body.
2. **`/v1/responses` drops inbound `tt_extras`** (`responses.rs:177` builds `tt_extras: HashMap::new()`), so the richer `tt_extras.panel` config (custom members/arbiter/quorum) never reaches the panel on that endpoint.

Phase 6 surfaces the panel attribution on both endpoints and fixes the `/v1/responses` `tt_extras` passthrough. **Cost attribution is already intact** (`x-tokentrimmer-*` headers pass through both transcoders verbatim). No billing change.

## 2. Key facts (verified in code)

- **Both transcoders call `chat::handler`** and transcode its `Response`: `/v1/messages` (`messages.rs:63` `handle`) and `/v1/responses` (`responses.rs:32` `handler`). Non-streaming → `transcode_json_response`; `/v1/messages` streaming → `transcode_sse_response`.
- **The panel header works on both** (it's read from headers in `prepare`, independent of the request body). The panel runs; the chat handler grafts `tokentrimmer.panel` into the JSON body (`chat.rs:2268-2286`) and attaches `x-tokentrimmer-*` headers.
- **Both transcoders pass the inbound `HeaderMap` verbatim to `chat::handler`** (`messages.rs:83`, `responses.rs:48`) → the `X-TokenTrimmer-Panel` header reaches `panel_from_header` in `prepare` and the panel runs. **Headers pass back through verbatim** too (`Response::from_parts`).
- **Cost-header asymmetry (corrected):** on **non-streaming** responses the full `x-tokentrimmer-*` cost headers are attached by `attach_cost_headers` (`chat.rs:2293`) and survive transcoding → non-streaming cost attribution is intact via headers. On **streaming** `/v1/messages`, `attach_sse_headers` (`sse.rs:1342-1349`) attaches **only** `x-tokentrimmer-trace-id` + `x-tokentrimmer-provider` — **no cost headers**. So for a streaming panel the cost is carried **only** by the `tokentrimmer.usage` SSE event (Phase 5). This makes forwarding that event (D4) **load-bearing for cost on streaming**, not a nicety. In all cases only the body-embedded `tokentrimmer.panel` object is what's lost.
- **Non-streaming drop point:** `messages.rs::transcode_json_response` (119-147) → `chat_response_to_messages` (`crates/providers/anthropic/src/messages.rs:399`) and `responses.rs::transcode_json_response` (616-638) → `chat_response_to_responses_json` both parse `ChatCompletionResponse` from the body bytes, losing the top-level `tokentrimmer` key.
- **`/v1/responses` `tt_extras`:** `ResponsesRequest` has `#[serde(flatten, default)] extra: HashMap<String, Value>` (`responses.rs:107`); `into_chat_request` builds `tt_extras: HashMap::new()` (`:177`) and passes `extra` (`:178`). The fix sources `tt_extras` from `extra`.
- **`/v1/messages` `tt_extras`:** `MessagesRequest` (`crates/providers/anthropic/src/messages.rs:46`) has **no** flatten/extra field; `into_chat_request` hardcodes `tt_extras: Default::default()` (`:247`). tt_extras is therefore **not sendable via the Anthropic request body** — the panel is header-triggered here, so this endpoint is **verify-only** for config (no tt_extras work).
- **`/v1/responses` streaming:** rejected entirely (`responses.rs:validate_supported`, ~`:183`: "streaming /v1/responses is not supported yet"). Out of scope for streaming.
- **`/v1/messages` streaming:** `transcode_sse_response` (`messages.rs:198`) rebuilds the stream frame-by-frame; `process_openai_frame` (`messages.rs:258`) only recognizes `ChatCompletionChunk` + error frames and **silently drops** everything else — including the Phase-5 `tokentrimmer.panel` and `tokentrimmer.usage` SSE events.

## 3. Decisions (approved)

- **D1 — Render `tokentrimmer.panel` as a top-level vendor key** in BOTH transcoded JSON bodies, identical in shape to `/v1/chat/completions` (`{ "tokentrimmer": { "panel": <panel_body_json> } }`). Uniform cross-endpoint contract: a TT-aware client reads the same shape everywhere. **This mirrors what `/v1/chat/completions` already does today** (it grafts the same top-level `tokentrimmer` key), so it introduces no *new* contract risk — clients that already tolerate TT's chat-completions responses tolerate this. The major Anthropic/OpenAI SDKs ignore unknown response fields; a client configured with strict `deny_unknown_fields` would need to disable it (documented as a client requirement, same as for chat completions). (Alt: re-shape per API-native conventions — rejected, YAGNI, no consumer needs a translated shape and it forks the contract.)
- **D2 — Extract-before-parse, no `chat::handler` change.** Each non-streaming transcoder parses the buffered chat body once as `serde_json::Value`, plucks `["tokentrimmer"]["panel"]` (if present), runs the existing shape conversion, then grafts the plucked panel back as the top-level `tokentrimmer.panel` key on the new body. Localized to the transcoders; the body is already buffered for transcoding, so no extra cost beyond one `Value` parse. (Alt: a new wrapper type returned from `chat::handler` so transcoders receive `panel_body` typed — rejected as too invasive for the gain.)
- **D3 — `/v1/responses` `tt_extras` passthrough.** In `ResponsesRequest::into_chat_request`, source `tt_extras` from the inbound flattened `extra` map: if `extra` carries a `tt_extras` object (a `Map<String,Value>`), move it into the `ChatCompletionRequest.tt_extras` field (and remove it from `extra` so it isn't double-passed). Enables `tt_extras.panel` rich panel config on `/v1/responses`. The header trigger already works regardless.
- **D4 — `/v1/messages` streaming: forward `tokentrimmer.*` SSE events as-is.** Extend the `transcode_sse_response` frame loop to recognize frames whose `event:` line is `tokentrimmer.panel` / `tokentrimmer.usage` and pass them through unchanged into the Anthropic output stream (no translation to Anthropic-native event types — no consumer needs it; YAGNI). **This is load-bearing, not optional:** per §2, streaming responses carry NO `x-tokentrimmer-*` cost headers, so the forwarded `tokentrimmer.usage` event is the *only* channel for streamed panel cost (and `tokentrimmer.panel` the only channel for the per-leg breakdown). `/v1/responses` streaming stays unsupported (rejected today; out of scope).
- **D5 — Off-by-default (panel-exclusive vs. all-streams).** A non-panel request on either endpoint: (a) grafts NO `tokentrimmer.panel` body key (the graft only happens when the plucked value is `Some`); (b) emits NO `event: tokentrimmer.panel` SSE frame (panel-exclusive — appears only on panel streams); (c) the `tt_extras` passthrough only acts when an inbound `tt_extras` key is present. **However, `event: tokentrimmer.usage` IS forwarded on all priceable `/v1/messages` streams (panel and non-panel alike)** — this matches `/v1/chat/completions` behavior and is the intended cost-transparency parity. Before Phase 6 the transcoder dropped this frame, leaving streaming `/v1/messages` clients with no cost channel at all (streaming SSE responses carry no `x-tokentrimmer-cost-*` headers). This is not a regression; it is a deliberate improvement. The no-extra-frames claim in Invariant 1 applies specifically to `tokentrimmer.panel` (confirmed/locked during final Phase 6 review, 2026-06-23).

## 4. Architecture

```
/v1/responses (non-stream only)                 /v1/messages (stream or non-stream)
  ResponsesRequest.into_chat_request:             MessagesRequest.into_chat_request (header-triggered; no tt_extras field)
    tt_extras ← extra["tt_extras"]  ← D3
  → chat::handler (panel runs via header)        → chat::handler (panel runs via header)
  → Response (body has tokentrimmer.panel grafted, x-tokentrimmer-* headers)
       │                                              │
  transcode_json_response:                       NON-STREAM transcode_json_response:
    val = parse body as Value                       val = parse body as Value
    panel = val["tokentrimmer"]["panel"]  ← D2       panel = val["tokentrimmer"]["panel"]  ← D2
    out = chat_response_to_responses_json(...)       out = chat_response_to_messages(...)
    if panel: out["tokentrimmer"]["panel"]=panel     if panel: out["tokentrimmer"]["panel"]=panel
       (headers preserved → cost intact)              (headers preserved → cost intact)
                                                  STREAM transcode_sse_response:  ← D4
                                                    per frame: if `event: tokentrimmer.*` → forward verbatim
                                                               else → existing OpenAI→Anthropic transcode
```

## 5. Components & seams

### 5.1 `/v1/responses` tt_extras passthrough (`responses.rs:~130-178`, `into_chat_request`)
Before building the `ChatCompletionRequest`, pull `tt_extras` out of `self.extra`: if `extra.remove("tt_extras")` yields a `Value::Object(map)`, convert it to the `HashMap<String, Value>` the field expects and use it; else `HashMap::new()`. Removing it from `extra` prevents the same key also flowing through the generic `extra` passthrough. (Mirror how `metadata` is special-cased at `responses.rs:130-134`.)

### 5.2 Non-streaming panel render — `/v1/responses` (`responses.rs::transcode_json_response`, 616-638)
The conversion fn `chat_response_to_responses_json(&ChatCompletionResponse) -> Value` takes the typed struct and returns a `Value`, so the graft target is its **return value** and extraction needs a **two-parse** of the buffered bytes (the redundant parse is acceptable — the body is small and already buffered):
1. `let val: serde_json::Value = serde_json::from_slice(&bytes)?;` then `let panel = val.get("tokentrimmer").and_then(|t| t.get("panel")).cloned();`
2. `let chat: ChatCompletionResponse = serde_json::from_slice(&bytes)?;` (existing line, unchanged) → `let mut out = chat_response_to_responses_json(&chat);`
3. `if let Some(p) = panel { if let Some(obj) = out.as_object_mut() { obj.insert("tokentrimmer".into(), json!({ "panel": p })); } }`
Re-attach the original status + headers unchanged. (Equivalently, step 2 may use `serde_json::from_value(val.clone())` to avoid a second `from_slice`; either is fine.)

### 5.3 Non-streaming panel render — `/v1/messages` (`messages.rs::transcode_json_response`, 119-147)
Identical two-parse pattern against `chat_response_to_messages(&ChatCompletionResponse) -> Value` (`crates/providers/anthropic/src/messages.rs:399`): parse the buffered bytes as `Value` to pluck `tokentrimmer.panel`, run the existing typed conversion, then `out.as_object_mut().insert("tokentrimmer", { "panel": ... })` when the pluck is `Some`. Preserve status + headers.

### 5.4 `/v1/messages` streaming forward (`messages.rs::transcode_sse_response` 198-245 / `process_openai_frame` 258)
**Parser gap to close:** `process_openai_frame` today iterates only `data:` lines (`messages.rs:260-267` skips every non-`data:` line via `continue`) and never inspects the `event:` line, so a Phase-5 panel frame — which is emitted as `event: tokentrimmer.panel\ndata: {...}` (`Event::default().event("tokentrimmer.panel")` in `sse.rs`) — is parsed as a frame with no recognizable `data:` chunk and dropped (returns `None`). Fix: **at the top of `process_openai_frame`, before the data-line loop**, inspect the raw frame text for an `event: tokentrimmer.` line; if present, return the **entire frame verbatim** (its `event:` + `data:` lines) without attempting `ChatCompletionChunk`/error parsing. Otherwise fall through to today's behavior. This preserves the Phase-5 terminal ordering (content chunks → `tokentrimmer.panel` → `tokentrimmer.usage` → `[DONE]`). `[DONE]` handling is unchanged.

### 5.5 No `chat::handler` / `panel.rs` / `sse.rs` changes
The chat-side panel production (Phases 1–5) is reused as-is. Phase 6 is entirely in the two transcoder modules + the two `into_*_request` translators. `panel_body_json` remains the single source of truth for the panel shape (the transcoders just relay the already-serialized object).

## 6. Invariants (targeted by tests)
1. **Off-by-default / panel-exclusive.** No panel header ⇒ (a) the transcoded `/v1/messages` and `/v1/responses` bodies carry NO `tokentrimmer` key (`tokentrimmer.panel` grafted only when pluck is `Some`); (b) NO `event: tokentrimmer.panel` SSE frame is emitted (panel-exclusive). **Note:** `event: tokentrimmer.usage` IS forwarded on all priceable `/v1/messages` streams regardless of panel, matching `/v1/chat/completions` — this is intentional cost-transparency parity, not a regression (before Phase 6 streaming `/v1/messages` had no cost channel). Non-streaming non-panel bodies remain byte-identical to today's transcoded output; the only Phase 6 delta is the conditional `tokentrimmer.panel` key. (Confirmed/locked during final Phase 6 review, 2026-06-23.)
2. **Cost preserved (per transport).** Non-streaming transcoded responses keep the full `x-tokentrimmer-*` cost headers (they already pass through). Streaming `/v1/messages` carries no cost headers today (§2) — Phase 6 keeps cost reaching the client by **forwarding the `tokentrimmer.usage` SSE event** (D4); without D4 the streamed panel cost would be silently dropped.
3. **Panel rendered (non-stream).** A panel request to `/v1/messages` and `/v1/responses` returns the target-API body with a top-level `tokentrimmer.panel` object equal to the one `/v1/chat/completions` returns for the same panel.
4. **tt_extras passthrough.** A `/v1/responses` request whose body carries `tt_extras: { panel: {...} }` reaches the panel as that config (e.g. custom `members`/`arbiter`), verified by the rendered panel reflecting it.
5. **Streaming forward (`/v1/messages`).** A streaming panel request to `/v1/messages` yields an Anthropic SSE stream that still contains the `tokentrimmer.panel` and `tokentrimmer.usage` events (forwarded verbatim), in order, alongside the transcoded Anthropic content events.
6. **No billing change.** request_logs / spend / served are produced entirely by `chat::handler`; Phase 6 touches none of it.

## 7. Testing (TDD)
- **`/v1/responses` panel render** (router test, mock panel providers, `X-TokenTrimmer-Panel: synthesize`): assert the Responses-API body has top-level `tokentrimmer.panel` with `legs[]` + `arbiter.strategy`, and `x-tokentrimmer-cost-usd` header present.
- **`/v1/responses` tt_extras passthrough**: a request body with `tt_extras: { panel: { members: [...], arbiter: ... } }` ⇒ the rendered `tokentrimmer.panel.legs` reflect those members (proves `tt_extras` reached the panel).
- **`/v1/messages` panel render (non-stream)**: same as the Responses test against the Anthropic body shape.
- **`/v1/messages` streaming forward**: a streaming panel request ⇒ the Anthropic SSE output contains `event: tokentrimmer.panel` and `event: tokentrimmer.usage` frames (verbatim) plus the Anthropic content events; ordering preserved.
- **Off-by-default regression (both endpoints)**: a no-panel request ⇒ transcoded body has NO `tokentrimmer` key and the existing `/v1/messages` + `/v1/responses` test suites stay green (byte-identical). A no-panel `/v1/messages` stream ⇒ no extra frames.
- **Cost header regression**: `x-tokentrimmer-*` headers present on a transcoded panel response.

## 8. Out of scope
- `/v1/responses` streaming (rejected today; not enabled here).
- Translating `tokentrimmer.*` SSE events to Anthropic-native event types (forward-as-is only; no consumer needs translation).
- Adding a `tt_extras` field to the Anthropic `MessagesRequest` body (panel is header-triggered on `/v1/messages`; verify-only per the master spec).
- Any change to billing, the panel engine, `chat::handler`, or `sse.rs` (Phases 1–5 reused unchanged).
- Phase 7 (CallerTier entitlement gate + `RouteAction.panel` org config + gateway-API-reference docs + the agent-loop `record_request_served` unify).

## 9. Self-review
- **Placeholders:** none — every seam cites a verified file:line; the graft/pluck and tt_extras-from-`extra` mechanics are concrete.
- **Consistency:** one rendering shape (`tokentrimmer.panel` top-level) across all three ingresses; the transcoders relay `panel_body_json`'s output without re-deriving it. Cost stays in headers (unchanged). No money-path code touched.
- **Scope:** two transcoder modules + two translators; one plan, no cloud change, no chat/sse/panel change.
- **Ambiguity:** render shape (D1), extraction mechanism (D2), tt_extras source (D3), streaming forward vs translate (D4), and off-by-default (D5) are each pinned to one behavior with the alternative noted/rejected.
- **Review hardening (2-lens adversarial pass):** corrected the cost-header premise — streaming `/v1/messages` has **no** `x-tokentrimmer-*` cost headers (`attach_sse_headers` sets only trace-id+provider), so D4's `tokentrimmer.usage` forwarding is **load-bearing for streamed cost**, not optional (was wrongly flagged "first to cut"). Pinned the **two-parse** extraction (the conversion fns take `&ChatCompletionResponse` and return a `Value` — graft onto the return value). Pinned the **`event:`-line detection** gap in `process_openai_frame` (it only reads `data:` lines today). Added the `deny_unknown_fields` client note (same posture as existing chat-completions responses) and reworded the off-by-default invariant ("unchanged from today's transcoded output," not byte-identical to the chat body). The other "needs-rework" findings were the reviewers confirming the *current* drop behavior that Phase 6 exists to fix — the recommended fixes matched the design.
