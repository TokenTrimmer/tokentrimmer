"""Negative: model from environment variable, cannot determine if flagship."""
import openai
import os

client = openai.OpenAI()
response = client.chat.completions.create(
    model=os.getenv("LLM_MODEL", "gpt-4o-mini"),
    messages=[{"role": "user", "content": "Summarise this text for me."}],
)
