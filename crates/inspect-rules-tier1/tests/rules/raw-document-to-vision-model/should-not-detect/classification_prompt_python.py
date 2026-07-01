import openai

client = openai.OpenAI()

resp = client.chat.completions.create(
    model="gpt-4o-mini",
    messages=[
        {"role": "user", "content": "Classify this ticket as bug/feature/question."},
    ],
)
label = resp.choices[0].message.content
print(label)
