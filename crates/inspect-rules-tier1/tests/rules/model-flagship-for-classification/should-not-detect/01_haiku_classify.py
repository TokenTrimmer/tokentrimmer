"""Negative: using a small model (Haiku) for classification — fine."""
import anthropic

client = anthropic.Anthropic()
result = client.messages.create(
    model="claude-3-haiku-20240307",
    max_tokens=10,
    messages=[{"role": "user", "content": "classify: positive or negative? I love this!"}],
)
