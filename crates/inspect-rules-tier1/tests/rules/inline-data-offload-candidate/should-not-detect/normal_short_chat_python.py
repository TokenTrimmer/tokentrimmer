import openai

resp = openai.chat.completions.create(
    model="gpt-4o",
    max_tokens=64,
    messages=[
        {"role": "system", "content": "You are a helpful assistant."},
        {"role": "user", "content": "What is the capital of France?"},
    ],
)
print(resp)
