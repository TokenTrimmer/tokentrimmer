"""Positive: from openai import — triggers rule."""
from openai import OpenAI

client = OpenAI()
r = client.chat.completions.create(
    model="gpt-4o",
    max_tokens=100,
    messages=[{"role": "user", "content": "Test"}],
)
