import openai

resp = openai.chat.completions.create(
    model="gpt-4o",
    max_tokens=50,
    messages=[
        {"role": "user", "content": """a,b,c
1,2,3
4,5,6"""},
    ],
)
print(resp)
