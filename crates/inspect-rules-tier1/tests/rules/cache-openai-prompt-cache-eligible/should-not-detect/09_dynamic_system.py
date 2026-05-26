"""Negative: system is short and dynamically constructed — under threshold."""
import openai

client = openai.OpenAI()

user_name = "Alice"
system_msg = f"You are a helpful assistant for {user_name}."

response = client.chat.completions.create(
    model="gpt-4o",
    messages=[
        {"role": "system", "content": system_msg},
        {"role": "user", "content": "Help me."},
    ],
)
