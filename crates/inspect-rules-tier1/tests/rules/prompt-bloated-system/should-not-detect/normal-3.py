import anthropic

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-3-haiku-20240307",
    messages=[{"role": "user", "content": "test"}]
)
