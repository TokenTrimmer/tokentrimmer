"""Negative: GPT-4o for complex reasoning about strategy."""
import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4o",
    max_tokens=4096,
    messages=[
        {"role": "system", "content": "You are a strategic advisor."},
        {"role": "user", "content": "Analyse this business situation and recommend a strategy."},
    ],
)
