"""Negative: uses max_completion_tokens (o-model style)."""
import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="o3-mini",
    max_completion_tokens=1000,
    messages=[{"role": "user", "content": "Solve this math problem."}],
)
