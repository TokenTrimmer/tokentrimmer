"""Negative: Claude Sonnet for code generation — not a classification task."""
import anthropic

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-3-5-sonnet-20241022",
    max_tokens=2048,
    messages=[
        {"role": "user", "content": "Write a Python function to sort a list of dicts by key."},
    ],
)
