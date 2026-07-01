import anthropic

client = anthropic.Anthropic()

resp = client.messages.create(
    model="claude-3-5-sonnet",
    max_tokens=1024,
    messages=[
        {
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "iVBORw0KGgoAAAANSUhEUgAA",
                    },
                },
                {"type": "text", "text": "Summarize the chart in this screenshot."},
            ],
        }
    ],
)
print(resp)
