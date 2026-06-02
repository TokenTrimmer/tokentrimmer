import anthropic

client = anthropic.Anthropic()

short_system = "You are a helpful assistant."

response = client.messages.create(
    model="claude-3-haiku-20240307",
    system=short_system,
    messages=[{"role": "user", "content": "Hello"}]
)
