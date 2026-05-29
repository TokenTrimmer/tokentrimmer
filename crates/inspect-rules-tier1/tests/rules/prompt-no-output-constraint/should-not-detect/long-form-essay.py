import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4-turbo",
    messages=[
        {
            "role": "user",
            "content": "Write a detailed essay about climate change"
        }
    ]
)
