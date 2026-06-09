"""Streaming chat. The SDK returns the raw stream (no .tt); read usage off the
final chunk by requesting stream_options={'include_usage': True}."""

import os

from tokentrimmer import TokenTrimmer

client = TokenTrimmer(api_key=os.environ["TOKENTRIMMER_API_KEY"])

stream = client.chat.completions.create(
    model="claude-haiku-4-5",
    messages=[{"role": "user", "content": "Count to five."}],
    stream=True,
    stream_options={"include_usage": True},
)

usage = None
for chunk in stream:
    if chunk.choices and chunk.choices[0].delta.content:
        print(chunk.choices[0].delta.content, end="", flush=True)
    if getattr(chunk, "usage", None):
        usage = chunk.usage
print()
print(f"usage: {usage}")
