import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="gpt-4o",
    top_n=3,
    messages=[{"role": "user", "content": "top_n is not n"}],
)
