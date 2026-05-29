import anthropic

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-3-haiku-20240307",
    messages=[
        {
            "role": "user",
            "content": "Write a helpful response: How do I use this library?"
        }
    ]
)
