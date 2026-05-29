import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="gpt-4o",
    min_p=2,
    messages=[{"role": "user", "content": "min not n"}],
)
