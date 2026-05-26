"""Negative: conversation with sliding window pruning."""
import openai

client = openai.OpenAI()
messages = []

def chat(user_input: str) -> str:
    messages.append({"role": "user", "content": user_input})
    # Prune to last 20 messages
    if len(messages) > 20:
        messages = messages[-20:]
    response = client.chat.completions.create(model="gpt-4o", messages=messages)
    reply = response.choices[0].message.content
    messages.append({"role": "assistant", "content": reply})
    return reply
