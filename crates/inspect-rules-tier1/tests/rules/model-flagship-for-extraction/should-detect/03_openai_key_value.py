"""Positive: GPT-4o for key-value extraction."""
from openai import OpenAI

client = OpenAI()

def extract_fields(doc: str) -> str:
    r = client.chat.completions.create(
        model="gpt-4o",
        messages=[
            {"role": "user", "content": f"Extract key-value pairs from: {doc}"},
        ],
    )
    return r.choices[0].message.content
