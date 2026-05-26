"""Positive: from-import style, messages.create present, no prompt annotation."""
from anthropic import Anthropic

CLIENT = Anthropic()

response = CLIENT.messages.create(
    model="claude-3-opus-20240229",
    max_tokens=2048,
    messages=[{"role": "user", "content": "Explain quantum computing."}],
)
print(response.content[0].text)
