import openai

client = openai.OpenAI()

# gpt-3.5-turbo is text-only (not vision-capable) -> out of scope for this rule.
resp = client.chat.completions.create(
    model="gpt-3.5-turbo",
    messages=[
        {
            "role": "user",
            "content": [
                {"type": "text", "text": "ignored"},
                {
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAA"},
                },
            ],
        }
    ],
)
print(resp)
