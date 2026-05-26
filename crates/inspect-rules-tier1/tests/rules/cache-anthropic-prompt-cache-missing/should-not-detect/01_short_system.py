"""Negative: system prompt is short (well under 1024 tokens)."""
import anthropic

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-3-5-sonnet-20241022",
    max_tokens=256,
    system="You are a helpful assistant.",
    messages=[{"role": "user", "content": "Hello"}],
)
