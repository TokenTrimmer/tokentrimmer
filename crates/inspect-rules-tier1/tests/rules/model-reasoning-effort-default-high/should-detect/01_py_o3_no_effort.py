import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="o3",
    messages=[{"role": "user", "content": "Solve this."}],
)
