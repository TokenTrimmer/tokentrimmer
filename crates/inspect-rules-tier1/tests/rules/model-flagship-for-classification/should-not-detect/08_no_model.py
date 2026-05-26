"""Negative: no model string — can't be flagged."""
import openai

client = openai.OpenAI()
# model comes from config variable, can't detect
model_name = get_model_config()
response = client.chat.completions.create(
    model=model_name,
    messages=[{"role": "user", "content": "Is this spam or not?"}],
)
