"""Negative: uses messages[-N:] slicing — pruned."""
import anthropic

client = anthropic.Anthropic()
history = []

def turn(msg: str) -> str:
    history.append({"role": "user", "content": msg})
    trimmed = history[-10:]
    resp = client.messages.create(
        model="claude-3-5-sonnet-20241022",
        max_tokens=512,
        messages=trimmed,
    )
    history.append({"role": "assistant", "content": resp.content[0].text})
    return resp.content[0].text
