import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="o4-mini",
    reasoning_effort="high",
    messages=[{"role": "user", "content": "Hard problem."}],
)
