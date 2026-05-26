"""Negative: GPT-4o for a complex long-form task — no classification keywords."""
import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4o",
    max_tokens=4096,
    messages=[
        {"role": "system", "content": "You are a thorough technical writer."},
        {"role": "user", "content": "Write a comprehensive guide to Kubernetes networking."},
    ],
)
