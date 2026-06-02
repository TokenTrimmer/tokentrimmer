import openai
client = openai.OpenAI()
resp = client.responses.create(
    model="o3",
    reasoning_effort="high",
    input="Prove the theorem.",
)
