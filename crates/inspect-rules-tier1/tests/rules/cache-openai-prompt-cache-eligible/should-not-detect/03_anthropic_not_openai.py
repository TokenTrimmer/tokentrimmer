"""Negative: Anthropic SDK call, not OpenAI — rule is OpenAI-specific."""
import anthropic

client = anthropic.Anthropic()
LONG = "x" * 5000
response = client.messages.create(
    model="claude-3-5-sonnet-20241022",
    max_tokens=1024,
    system=LONG,
    messages=[{"role": "user", "content": "Hello"}],
)
