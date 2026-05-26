"""Positive: OpenAI call without max_tokens."""
import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4o",
    messages=[
        {"role": "system", "content": "You are helpful."},
        {"role": "user", "content": "Write a comprehensive report."},
    ],
)
