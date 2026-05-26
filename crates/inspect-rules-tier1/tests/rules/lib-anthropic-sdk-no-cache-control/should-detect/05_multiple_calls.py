"""Positive: multiple messages.create calls, no prompt-cache annotation in file."""
import anthropic

c = anthropic.Anthropic()

r1 = c.messages.create(
    model="claude-3-haiku-20240307",
    max_tokens=100,
    messages=[{"role": "user", "content": "Classify this: happy"}],
)

r2 = c.messages.create(
    model="claude-3-5-sonnet-20241022",
    max_tokens=500,
    messages=[{"role": "user", "content": "Summarise this document."}],
)
