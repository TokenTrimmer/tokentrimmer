comment = "I used text-davinci-003 before but now it's deprecated"
deprecated_model_name = "gpt-3.5-turbo-0301"

# This code uses current models
import openai
client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4",
    messages=[{"role": "user", "content": "Hello"}]
)
