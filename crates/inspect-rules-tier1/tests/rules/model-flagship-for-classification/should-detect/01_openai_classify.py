"""Positive: GPT-4o used to classify sentiment."""
import openai

client = openai.OpenAI()

def classify_sentiment(text: str) -> str:
    response = client.chat.completions.create(
        model="gpt-4o",
        max_tokens=10,
        messages=[
            {"role": "system", "content": "You classify sentiment."},
            {"role": "user", "content": f"Is this positive or negative? {text}"},
        ],
    )
    return response.choices[0].message.content

# This clearly calls classify on a short prompt with a flagship model.
