import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="gpt-5.5",
    messages=[{"role": "user", "content": "flagship chat"}],
)
