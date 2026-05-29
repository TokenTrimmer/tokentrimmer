import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model='o3',
    reasoning_effort='low',
    messages=[{"role": "user", "content": "q"}],
)
