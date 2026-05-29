import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4-0314",
    messages=[{"role": "user", "content": "Hello"}]
)
