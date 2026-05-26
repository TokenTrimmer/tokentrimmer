"""Positive: GPT-4o used to label categories."""
from openai import OpenAI

client = OpenAI()

CATEGORIES = ["tech", "sports", "politics", "entertainment"]

def categorize(headline: str) -> str:
    resp = client.chat.completions.create(
        model="gpt-4o",
        messages=[
            {"role": "user", "content": f"Categorize this news headline: {headline}"},
        ],
    )
    return resp.choices[0].message.content
