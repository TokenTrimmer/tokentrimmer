"""Positive: GPT-4o for structured extraction."""
import openai

client = openai.OpenAI()

def extract_entities(text: str) -> dict:
    resp = client.chat.completions.create(
        model="gpt-4o",
        messages=[
            {"role": "system", "content": "Extract named entities as JSON."},
            {"role": "user", "content": text},
        ],
    )
    return resp.choices[0].message.content
