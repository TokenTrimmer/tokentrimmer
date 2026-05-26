"""Positive: Anthropic conversation with unbounded history.append."""
import anthropic

client = anthropic.Anthropic()
history = []

def turn(user_message: str) -> str:
    history.append({"role": "user", "content": user_message})
    resp = client.messages.create(
        model="claude-3-5-sonnet-20241022",
        max_tokens=1024,
        messages=history,
    )
    assistant_text = resp.content[0].text
    history.append({"role": "assistant", "content": assistant_text})
    return assistant_text
