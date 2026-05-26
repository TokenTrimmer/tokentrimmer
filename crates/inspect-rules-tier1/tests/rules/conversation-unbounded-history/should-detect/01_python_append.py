"""Positive: OpenAI conversation with unbounded messages.append."""
import openai

client = openai.OpenAI()
messages = [{"role": "system", "content": "You are helpful."}]

def chat(user_input: str) -> str:
    messages.append({"role": "user", "content": user_input})
    response = client.chat.completions.create(
        model="gpt-4o",
        messages=messages,
    )
    reply = response.choices[0].message.content
    messages.append({"role": "assistant", "content": reply})
    return reply
