"""Negative: anthropic imported but only uses streaming API, not the create endpoint."""
import anthropic

client = anthropic.Anthropic()

with client.messages.stream(
    model="claude-3-5-sonnet-20241022",
    max_tokens=100,
    messages=[{"role": "user", "content": "Hi"}],
) as stream:
    pass
