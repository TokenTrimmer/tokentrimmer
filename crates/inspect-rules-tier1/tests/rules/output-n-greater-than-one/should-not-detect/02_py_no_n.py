import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="gpt-4o",
    max_tokens=128,
    messages=[{"role": "user", "content": "no n here"}],
)
