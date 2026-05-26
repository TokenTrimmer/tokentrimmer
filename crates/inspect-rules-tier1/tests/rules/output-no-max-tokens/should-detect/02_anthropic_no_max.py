"""Positive: Anthropic call without max_tokens."""
import anthropic

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-3-5-sonnet-20241022",
    messages=[{"role": "user", "content": "Summarise the universe."}],
)
