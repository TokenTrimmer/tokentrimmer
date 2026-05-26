"""Negative: cache_control is present — correctly annotated."""
import anthropic

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-3-5-sonnet-20241022",
    max_tokens=1024,
    system=[{"type": "text", "text": "You are helpful.", "cache_control": {"type": "ephemeral"}}],
    messages=[{"role": "user", "content": "Hello"}],
)
