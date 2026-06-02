import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="gpt-4o",
    n=1,
    messages=[{"role": "user", "content": "one"}],
)
