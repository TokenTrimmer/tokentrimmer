"""Negative: max_history variable signals pruning awareness."""
import openai

client = openai.OpenAI()
messages = []
max_history = 20

def chat(user_input: str) -> str:
    messages.append({"role": "user", "content": user_input})
    if len(messages) > max_history:
        messages.pop(0)
        messages.pop(0)
    response = client.chat.completions.create(model="gpt-4o", messages=messages)
    reply = response.choices[0].message.content
    messages.append({"role": "assistant", "content": reply})
    return reply
