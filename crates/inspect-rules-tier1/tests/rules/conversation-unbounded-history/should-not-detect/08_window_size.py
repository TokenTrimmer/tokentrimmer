"""Negative: window_size variable indicates pruning logic."""
import anthropic

client = anthropic.Anthropic()
window_size = 15
history = []

def ask(msg: str):
    history.append({"role": "user", "content": msg})
    resp = client.messages.create(
        model="claude-3-5-sonnet-20241022",
        max_tokens=1024,
        messages=history[-window_size:],
    )
    history.append({"role": "assistant", "content": resp.content[0].text})
