"""Negative: messages.create call with no system= argument at all."""
import anthropic

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-3-5-sonnet-20241022",
    max_tokens=512,
    messages=[{"role": "user", "content": "Write me a poem about cats."}],
)
