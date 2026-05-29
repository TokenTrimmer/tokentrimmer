import openai
client = openai.OpenAI()
resp = client.chat.completions.create(
    model="gpt-4o",
    tools=[{"type": "function", "function": {"name": "f"}}],
    messages=[{"role": "user", "content": "Call f."}],
)
