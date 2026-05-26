"""Negative: OpenAI imported but no completions.create call."""
from openai import OpenAI

client = OpenAI()
# Using embeddings, not chat completions
embedding = client.embeddings.create(
    model="text-embedding-3-small",
    input="Hello world",
)
