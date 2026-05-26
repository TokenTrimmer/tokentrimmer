"""Negative: OpenAI SDK — not an Anthropic call, rule should not fire."""
import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4o",
    max_tokens=512,
    messages=[
        {"role": "system", "content": "You are helpful. " + "x" * 5000},
        {"role": "user", "content": "Hello"},
    ],
)
