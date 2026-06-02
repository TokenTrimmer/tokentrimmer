import os
import openai

# Model from environment variable
model = os.getenv("LLM_MODEL", "gpt-4-turbo")
client = openai.OpenAI()
response = client.chat.completions.create(
    model=model,
    messages=[{"role": "user", "content": "Hello"}]
)
