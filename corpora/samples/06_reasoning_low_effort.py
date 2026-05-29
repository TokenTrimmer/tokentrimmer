"""Reasoning model used deliberately with low effort for a simple task."""
from openai import OpenAI

client = OpenAI()

def quick_reason(question: str):
    return client.chat.completions.create(
        model="o3",
        reasoning_effort="low",
        max_completion_tokens=512,
        messages=[{"role": "user", "content": question}],
    )
