import openai

client = openai.OpenAI()

# Pipe a raw screenshot straight into the vision model on every run.
response = client.chat.completions.create(
    model="gpt-4o",
    max_tokens=500,
    messages=[
        {
            "role": "user",
            "content": [
                {"type": "text", "text": "Transcribe every field in this form."},
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "data:image/jpeg;base64,/9j/4AAQSkZJRgABAQEAYABgAAD",
                        "detail": "high",
                    },
                },
            ],
        }
    ],
)
