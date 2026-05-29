import anthropic

client = anthropic.Anthropic()
response = client.messages.create(
    model="claude-3-haiku-20240307",
    max_tokens=100,
    messages=[
        {
            "role": "user",
            "content": "Classify this email as spam, ham, or promotional. Email: Hello, buy our products!"
        }
    ]
)
