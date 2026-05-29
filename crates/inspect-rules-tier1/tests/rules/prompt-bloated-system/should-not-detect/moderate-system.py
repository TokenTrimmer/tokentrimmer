import openai

client = openai.OpenAI()

system = "You are a helpful assistant. Respond concisely and accurately. Focus on being helpful."

response = client.chat.completions.create(
    model="gpt-4-turbo",
    system=system,
    messages=[{"role": "user", "content": "test"}]
)
