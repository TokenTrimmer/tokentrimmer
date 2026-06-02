import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="gpt-4o",
    max_tokens=200,
    messages=[{"role": "user", "content": "bounded"}],
)
