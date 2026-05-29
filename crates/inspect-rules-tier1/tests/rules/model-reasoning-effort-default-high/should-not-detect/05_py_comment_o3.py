import openai
client = openai.OpenAI()
# We evaluated o3 but chose gpt-4o for latency reasons.
resp = client.chat.completions.create(
    model="gpt-4o",
    messages=[{"role": "user", "content": "go"}],
)
