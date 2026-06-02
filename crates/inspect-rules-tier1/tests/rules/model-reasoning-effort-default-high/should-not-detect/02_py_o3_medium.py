import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="o3",
    reasoning_effort="medium",
    messages=[{"role": "user", "content": "mid"}],
)
