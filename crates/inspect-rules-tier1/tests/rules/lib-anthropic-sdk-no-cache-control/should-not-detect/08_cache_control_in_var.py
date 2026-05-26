"""Negative: cache_control defined as a variable and used in the call."""
import anthropic

CACHE = {"type": "ephemeral"}

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-3-5-sonnet-20241022",
    max_tokens=512,
    system=[{"type": "text", "text": "Be helpful.", "cache_control": CACHE}],
    messages=[{"role": "user", "content": "Go"}],
)
