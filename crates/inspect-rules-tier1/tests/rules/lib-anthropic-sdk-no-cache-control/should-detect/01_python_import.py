"""Positive: anthropic imported, messages.create called, no annotation present."""
import anthropic

client = anthropic.Anthropic()

def ask(question: str) -> str:
    response = client.messages.create(
        model="claude-3-5-sonnet-20241022",
        max_tokens=1024,
        system="You are a helpful assistant.",
        messages=[{"role": "user", "content": question}],
    )
    return response.content[0].text
