import openai

client = openai.OpenAI()

resp = client.chat.completions.create(
    model="gpt-4o",
    messages=[
        {
            "role": "user",
            "content": [
                {"type": "text", "text": "What is on this receipt?"},
                {
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAA"},
                },
            ],
        }
    ],
)
print(resp)
