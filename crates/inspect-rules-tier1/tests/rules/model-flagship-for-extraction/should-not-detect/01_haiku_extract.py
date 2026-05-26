"""Negative: Haiku for extraction — appropriate small model."""
import anthropic

client = anthropic.Anthropic()
result = client.messages.create(
    model="claude-3-haiku-20240307",
    max_tokens=128,
    system="Extract the JSON fields.",
    messages=[{"role": "user", "content": "Name: Alice, Age: 30"}],
)
