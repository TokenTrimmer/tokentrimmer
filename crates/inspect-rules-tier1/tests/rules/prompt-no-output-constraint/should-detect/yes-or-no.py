import openai

client = openai.OpenAI()
response = client.chat.completions.create(
    model="gpt-4-turbo",
    messages=[
        {
            "role": "user",
            "content": "Is this sentence grammatically correct? Yes or no only. Sentence: I goes to store"
        }
    ]
)
