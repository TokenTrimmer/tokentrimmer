# Deep Research Panel — Phase 6 (transcoder rendering) Design Spec

> Status: DRAFT (awaiting user review). Date: 2026-06-23. Repo: public. Branch: `feat/panel-phase6-transcoders`.
> Builds on Phases 1–5. Master spec: `2026-06-21-deep-research-panel-design.md` (roadmap row 6: "Render panel + per-leg attribution on `/v1/messages` + `/v1/responses`; fix `/v1/responses` `tt_extras` passthrough; verify `/v1/messages`").

## 1. Goal

The panel **already runs** on `/v1/messages` (Anthropic Messages API) and `/v1/responses` (OpenAI Responses API): both transcoders translate the inbound request to a `ChatCompletionRequest`, call the shared `chat::handler`, and the `X-TokenTrimmer-Panel` header (read from HTTP headers in `prepare`) triggers the panel exactly as on `/v1/chat/completions`. Two things are missing on these endpoints:
1. The **`tokentrimmer.panel` per-leg attribution object is dropped** — both transcoders re-parse the chat response body as `ChatCompletionResponse` (discarding the top-level `tokentrimmer` key the chat handler grafted in) and serialize a fresh target-shape body.
2. **`/v1/responses` drops inbound `tt_extras`** (`responses.rs:177` builds `tt_extras: HashMap::new()`), so the richer `tt_extras.panel` config (custom members/arbiter/quorum) never reaches the panel on that endpoint.

Phase 6 surfaces the panel attribution on both endpoints and fixes the `/v1/responses` `tt_extras` passthrough. **Cost attribution is already intact** (`x-tokentrimmer-*` headers pass through both transcoders verbatim). No billing change.

## 2. Key facts (verified in code)

- **Both transcoders call `chat::handler`** and transcode its `Response`: `/v1/messages` (`messages.rs:63` `handle`) and `/v1/responses` (`responses.rs:32` `handler`). Non-streaming → `transcode_json_response`; `/v1/messages` streaming → `transcode_sse_response`.
- **The panel header works on both** (it's read from headers in `prepare`, independent of the request body). The panel runs; the chat handler grafts `tokentrimmer.panel` into the JSON body (`chat.rs:2268-2286`) and attaches `x-tokentrimmer-*` headers.
- **Headers pass through both transcoders verbatim** (`Response::from_parts` preserves them) → **cost attribution is NOT lost**; only the body-embedded `tokentrimmer.panel` object is.
- **Non-streaming drop point:** `messages.rs::transcode_json_response` (119-147) → `chat_response_to_messages` (`crates/providers/anthropic/src/messages.rs:399`) and `responses.rs::transcode_json_response` (616-638) → `chat_response_to_responses_json` both parse `ChatCompletionResponse` from the body bytes, losing the top-level `tokentrimmer` key.
- **`/v1/responses` `tt_extras`:** `ResponsesRequest` has `#[serde(flatten, default)] extra: HashMap<String, Value>` (`responses.rs:107`); `into_chat_request` builds `tt_extras: HashMap::new()` (`:177`) and passes `extra` (`:178`). The fix sources `tt_extras` from `extra`.
- **`/v1/messages` `tt_extras`:** `MessagesRequest` (`crates/providers/anthropic/src/messages.rs:46`) has **no** flatten/extra field; `into_chat_request` hardcodes `tt_extras: Default::default()` (`:247`). tt_extras is therefore **not sendable via the Anthropic request body** — the panel is header-triggered here, so this endpoint is **verify-only** for config (no tt_extras work).
- **`/v1/responses` streaming:** rejected entirely (`responses.rs:validate_supported`, ~`:183`: "streaming /v1/responses is not supported yet"). Out of scope for streaming.
- **`/v1/messages` streaming:** `transcode_sse_response` (`messages.rs:198`) rebuilds the stream frame-by-frame; `process_openai_frame` (`messages.rs:258`) only recognizes `ChatCompletionChunk` + error frames and **silently drops** everything else — including the Phase-5 `tokentrimmer.panel` and `tokentrimmer.usage` SSE events.

## 3. Decisions (awaiting user approval)

- **D1 — Render `tokentrimmer.panel` as a top-level vendor key** in BOTH transcoded JSON bodies, identical in shape to `/v1/chat/completions` (`{ "tokentrimmer": { "panel": <panel_body_json> } }`). Uniform cross-endpoint contract: a TT-aware client reads the same shape everywhere; standard Anthropic/OpenAI clients ignore the unknown top-level key. (Alt: re-shape per API-native conventions — rejected, YAGNI, no consumer needs a translated shape and it forks the contract.)
- **D2 — Extract-before-parse, no `chat::handler` change.** Each non-streaming transcoder parses the buffered chat body once as `serde_json::Value`, plucks `["tokentrimmer"]["panel"]` (if present), runs the existing shape conversion, then grafts the plucked panel back as the top-level `tokentrimmer.panel` key on the new body. Localized to the transcoders; the body is already buffered for transcoding, so no extra cost beyond one `Value` parse. (Alt: a new wrapper type returned from `chat::handler` so transcoders receive `panel_body` typed — rejected as too invasive for the gain.)
- **D3 — `/v1/responses` `tt_extras` passthrough.** In `ResponsesRequest::into_chat_request`, source `tt_extras` from the inbound flattened `extra` map: if `extra` carries a `tt_extras` object (a `Map<String,Value>`), move it into the `ChatCompletionRequest.tt_extras` field (and remove it from `extra` so it isn't double-passed). Enables `tt_extras.panel` rich panel config on `/v1/responses`. The header trigger already works regardless.
- **D4 — `/v1/messages` streaming: forward `tokentrimmer.*` SSE events as-is.** Extend the `transcode_sse_response` loop to recognize frames whose `event:` line is `tokentrimmer.panel` / `tokentrimmer.usage` and pass them through unchanged into the Anthropic output stream (no translation to Anthropic-native event types — no consumer needs it; YAGNI). `/v1/responses` streaming stays unsupported (rejected today; out of scope).
- **D5 — Off-by-default / byte-identical.** A non-panel request on either endpoint produces a byte-identical transcoded body: the `tokentrimmer.panel` graft only happens when the plucked value is `Some`, and the SSE forwarding only triggers on `tokentrimmer.*` frames (which appear only for panels). The `tt_extras` passthrough only acts when an inbound `tt_extras` key is present.

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
After buffering the body bytes, parse once as `serde_json::Value`; `let panel = val.get("tokentrimmer").and_then(|t| t.get("panel")).cloned();`. Run the existing `chat_response_to_responses_json` conversion (it can deserialize the same bytes into `ChatCompletionResponse`, or take the `Value` — keep its current input). On the produced Responses-shape `Value`, if `panel.is_some()`, insert `out["tokentrimmer"] = json!({ "panel": panel })`. Re-attach the original status + headers unchanged.

### 5.3 Non-streaming panel render — `/v1/messages` (`messages.rs::transcode_json_response`, 119-147)
Same pattern: pluck `tokentrimmer.panel` from the buffered body `Value` before/alongside the `chat_response_to_messages` conversion, then graft `out["tokentrimmer"] = { "panel": ... }` onto the Anthropic-shape body when present. Preserve status + headers.

### 5.4 `/v1/messages` streaming forward (`messages.rs::transcode_sse_response` 198-245 / `process_openai_frame` 258)
In the frame loop, before discarding a frame that does not parse as `ChatCompletionChunk`, check whether its `event:` line names a `tokentrimmer.*` event (or the raw frame begins with `event: tokentrimmer.`). If so, emit the frame **verbatim** into the output stream (preserving the `event:`/`data:` lines and the Phase-5 ordering: chunks → `tokentrimmer.panel` → `tokentrimmer.usage` → `[DONE]`). All other unknown frames keep their current behavior. (`[DONE]` handling is unchanged from today.)

### 5.5 No `chat::handler` / `panel.rs` / `sse.rs` changes
The chat-side panel production (Phases 1–5) is reused as-is. Phase 6 is entirely in the two transcoder modules + the two `into_*_request` translators. `panel_body_json` remains the single source of truth for the panel shape (the transcoders just relay the already-serialized object).

## 6. Invariants (targeted by tests)
1. **Off-by-default / byte-identical.** No panel header ⇒ transcoded `/v1/messages` and `/v1/responses` bodies + streams are byte-identical to today (no `tokentrimmer` key grafted, no extra SSE frames).
2. **Cost preserved.** `x-tokentrimmer-*` headers remain present and unchanged on transcoded responses (regression: they already pass through).
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
- **Risk note:** the lowest-value piece is D4 (streaming forward on `/v1/messages`) — standard Anthropic clients ignore `tokentrimmer.*` events, so it only benefits TT-aware clients on that endpoint; it is cheap and keeps the metadata honest (no silent drop), but is the first candidate to cut if scope must shrink.
