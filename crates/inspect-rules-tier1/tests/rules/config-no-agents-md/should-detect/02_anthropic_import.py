"""Positive: imports anthropic without any AGENTS.md in sight."""
import anthropic

client = anthropic.Anthropic()
r = client.messages.create(
    model="claude-3-5-sonnet-20241022",
    max_tokens=512,
    messages=[{"role": "user", "content": "Hello"}],
)
