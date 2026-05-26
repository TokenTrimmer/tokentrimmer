"""Negative: streaming call with cache_control correctly applied."""
import anthropic

client = anthropic.Anthropic()
LONG = "x" * 5000

with client.messages.stream(
    model="claude-3-5-sonnet-20241022",
    max_tokens=1024,
    system=[{"type": "text", "text": LONG, "cache_control": {"type": "ephemeral"}}],
    messages=[{"role": "user", "content": "Hello"}],
) as stream:
    for text in stream.text_stream:
        print(text, end="", flush=True)
