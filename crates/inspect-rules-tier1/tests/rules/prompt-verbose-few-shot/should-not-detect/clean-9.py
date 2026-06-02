import anthropic

system = "You are a helpful assistant."

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-3-haiku-20240307",
    system=system,
    messages=[{"role": "user", "content": "Hello"}]
)
