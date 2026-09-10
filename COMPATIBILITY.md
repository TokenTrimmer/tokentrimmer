# TokenTrimmer Compatibility Matrix

This document maps TokenTrimmer's API surface, provider support, and SDK
availability. Check this before assuming an OpenAI-SDK integration is
drop-in compatible — "change the base URL" covers the tested endpoints
below, not the entire OpenAI API.

## API endpoint compatibility

| OpenAI endpoint | TokenTrimmer | Notes |
|---|---|---|
| `POST /v1/chat/completions` | ✅ Full | Buffered + streaming. Extra `X-TokenTrimmer-*` response headers. |
| `GET /v1/models` | ✅ Full | Includes TokenTrimmer extension object (provider, pricing, capabilities). |
| `POST /v1/embeddings` | ✅ Full | Multi-provider routing supported. |
| `POST /v1/messages` | ✅ Full | Anthropic-native Messages ingress (chat pipeline reuse). |
| `POST /v1/responses` | ⚠️ Partial | Non-streaming + stateless mode only. See [Responses API](docs/04-gateway-api-reference.md) for the exact gaps. |
| `POST /v1/files` | ✅ Full | OpenAI-compatible file upload for Batch API. |
| `POST /v1/batches` | ✅ Full | OpenAI-compatible async Batch API with per-model catalog rates. |
| `GET /v1/batches/:id` | ✅ Full | Status, completion, and cancel. |
| Other OpenAI endpoints | ❌ Not implemented | Audio, images, moderations, fine-tuning, assistants, threads. |

## Provider support

| Provider | Adapter | Chat | Streaming | Embeddings | Notes |
|---|---|---|---|---|---|
| OpenAI | `tt-provider-openai` | ✅ | ✅ | ✅ | Full SDK parity. Includes Flex service tier + Batch API rates. |
| Anthropic | `tt-provider-anthropic` | ✅ | ✅ | ❌ | Native Messages API. Prompt caching (cache_read + cache_write rates). |
| Google Gemini | `tt-provider-gemini` | ✅ | ✅ | ✅ | Gemini 2.0/2.5 series. |
| Mistral | `tt-provider-mistral` | ✅ | ✅ | ✅ | Mistral Large/Medium/Small. |
| Groq | `tt-provider-groq` | ✅ | ✅ | ✅ | Llama/Mixtral families. Low latency. |
| Together | `tt-provider-together` | ✅ | ✅ | ❌ | OpenAI-compatible endpoint. |
| OpenRouter | `tt-provider-openrouter` | ✅ | ✅ | ✅ | Multi-model aggregator passthrough. |
| Azure OpenAI | `tt-provider-azure` | ✅ | ✅ | ✅ | Azure-specific authentication. |
| Local | `tt-provider-local` | ✅ | ✅ | ✅ | Ollama-compatible. Always free (zero rates). |

## SDK availability

| Language | Package | Status | Installation |
|---|---|---|---|
| Rust | `tokentrimmer-client` | ✅ Published (crates.io) | `cargo add tokentrimmer-client` |
| Python | `tokentrimmer` | ❌ Not on PyPI (in development) | `pip install github:TokenTrimmer/tokentrimmer#subdirectory=sdk-python` |
| TypeScript | `@tokentrimmer/client` | ❌ Not on npm (in development) | `npm install github:TokenTrimmer/tokentrimmer#directory=sdk-typescript` |

## "Change the base URL" scope

The OpenAI-compatible API means existing OpenAI SDK users can point their
SDK at TokenTrimmer's gateway by changing the base URL:
`https://api.tokentrimmer.com/v1` (or your self-hosted URL). This works for:

- `/v1/chat/completions` (buffered + streaming)
- `/v1/models`
- `/v1/embeddings`

It does **not** automatically work for:
- Responses API (partial — stateless mode only, no streaming)
- Audio, images, or moderation endpoints (not implemented)
- Assistants, threads, or fine-tuning (not implemented)

If your integration uses these endpoints, route them directly to your
provider's API and use TokenTrimmer for the supported paths.

## Known limitations

- **Streaming Responses API**: Not implemented. Use `chat/completions` with
  `stream: true` instead.
- **Stateful Responses**: Background mode, function-calling state, and
  conversation threads in Responses API are not supported.
- **Realtime API**: Not implemented.
- **Audio/Transcription**: Not implemented.
- **Cross-provider routingwrites**: Route targets must share the source
  request's provider for the response to be understood correctly. The
  gateway's cross-provider routing is intentionally limited while pricing
  parity is established.
