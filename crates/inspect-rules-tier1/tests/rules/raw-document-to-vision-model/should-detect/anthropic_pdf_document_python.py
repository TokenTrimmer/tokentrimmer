import anthropic

client = anthropic.Anthropic()

resp = client.messages.create(
    model="claude-3-7-sonnet",
    max_tokens=2048,
    messages=[
        {
            "role": "user",
            "content": [
                {
                    "type": "document",
                    "source": {
                        "type": "base64",
                        "media_type": "application/pdf",
                        "data": "JVBERi0xLjcKJeLjz9MK",
                    },
                },
                {"type": "text", "text": "Extract the invoice line items."},
            ],
        }
    ],
)
print(resp)
