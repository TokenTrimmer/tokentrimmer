"""Negative: gpt-4o-mini for parsing — small model."""
import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4o-mini",
    messages=[{"role": "user", "content": "Parse this: key=value, foo=bar"}],
)
