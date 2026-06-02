import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="gpt-4o",
    temperature=2,
    messages=[{"role": "user", "content": "hot"}],
)
