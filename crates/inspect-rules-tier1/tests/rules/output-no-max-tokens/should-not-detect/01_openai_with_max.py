"""Negative: OpenAI call WITH max_tokens."""
import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4o",
    max_tokens=512,
    messages=[{"role": "user", "content": "Hello"}],
)
