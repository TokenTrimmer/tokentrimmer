"""Positive: file imports openai — in a repo with no AGENTS.md."""
import openai

client = openai.OpenAI()

def ask(q: str) -> str:
    r = client.chat.completions.create(
        model="gpt-4o",
        max_tokens=256,
        messages=[{"role": "user", "content": q}],
    )
    return r.choices[0].message.content
