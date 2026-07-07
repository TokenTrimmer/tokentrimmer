import openai

# Gemini via the OpenAI-compatible endpoint: model id is explicit, so D0's
# direction guard books a $0 saving -> the finding is suppressed.
client = openai.OpenAI(base_url="https://generativelanguage.googleapis.com/v1beta/openai/")

resp = client.chat.completions.create(
    model="gemini-2.5-flash",
    messages=[
        {
            "role": "user",
            "content": [
                {"type": "text", "text": "What is in this photo?"},
                {
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAA"},
                },
            ],
        }
    ],
)
print(resp)
