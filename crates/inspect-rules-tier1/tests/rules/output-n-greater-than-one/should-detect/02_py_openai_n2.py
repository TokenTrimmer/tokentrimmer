import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="gpt-4o-mini",
    messages=[{"role": "user", "content": "Variants?"}],
    n=2,
)
