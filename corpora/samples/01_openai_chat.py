"""Idiomatic OpenAI chat completion."""
from openai import OpenAI

client = OpenAI()

def summarise(text: str) -> str:
    resp = client.chat.completions.create(
        model="gpt-4o",
        max_tokens=256,
        messages=[
            {"role": "system", "content": "You summarise text concisely."},
            {"role": "user", "content": text},
        ],
    )
    return resp.choices[0].message.content
