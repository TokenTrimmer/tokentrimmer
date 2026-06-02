# Agent guide (corpus sample)

A short, representative agent-config file so the corpus looks like a real repo.

## Models
- Default: `gpt-4o` for chat, `gpt-4o-mini` for lightweight classification.
- Always set an explicit `max_tokens`.

## Caching
- Cache long Anthropic system prompts and tool schemas with `cache_control`.

## Agent loops
- Bound every loop with `max_iterations` and keep a sliding history window.
