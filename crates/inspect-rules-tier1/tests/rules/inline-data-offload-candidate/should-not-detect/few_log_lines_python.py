import openai

resp = openai.chat.completions.create(
    model="gpt-4o",
    max_tokens=80,
    messages=[
        {"role": "user", "content": """2026-01-01T12:00:00 INFO started
2026-01-01T12:00:01 INFO ready
2026-01-01T12:00:02 INFO done"""},
    ],
)
print(resp)
