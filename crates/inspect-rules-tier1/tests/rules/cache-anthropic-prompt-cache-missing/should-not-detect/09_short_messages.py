"""Negative: messages.create call with no system argument and short content."""
import anthropic

client = anthropic.Anthropic()

response = client.messages.create(
    model="claude-3-haiku-20240307",
    max_tokens=100,
    messages=[
        {"role": "user", "content": "Is Paris in France?"},
    ],
)
print(response.content[0].text)
