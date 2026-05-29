import openai
client = openai.OpenAI()
payload = client.chat.completions.create(**{
    "model": "gpt-4o",
    "n": 3,
    "messages": [{"role": "user", "content": "go"}],
})
