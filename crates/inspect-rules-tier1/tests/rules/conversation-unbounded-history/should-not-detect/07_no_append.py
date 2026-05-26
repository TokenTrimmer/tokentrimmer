"""Negative: has OpenAI import but never appends to a message list."""
import openai

client = openai.OpenAI()

def single_shot(prompt: str) -> str:
    resp = client.chat.completions.create(
        model="gpt-4o",
        max_tokens=256,
        messages=[{"role": "user", "content": prompt}],
    )
    return resp.choices[0].message.content
