"""Negative: GPT-4o-mini for classification — appropriate small model."""
import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4o-mini",
    max_tokens=10,
    messages=[
        {"role": "user", "content": "Is this positive or negative? Great day!"},
    ],
)
