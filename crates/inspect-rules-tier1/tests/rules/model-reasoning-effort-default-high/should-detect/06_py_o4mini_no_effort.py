import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="o4-mini",
    max_completion_tokens=2000,
    messages=[{"role": "user", "content": "Analyse."}],
)
