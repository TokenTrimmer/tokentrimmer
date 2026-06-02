import anthropic

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-2",
    max_tokens=1000,
    messages=[{"role": "user", "content": "Hello"}]
)
