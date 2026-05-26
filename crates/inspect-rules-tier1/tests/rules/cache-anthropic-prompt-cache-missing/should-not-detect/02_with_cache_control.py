"""Negative: long system prompt WITH cache_control — correctly annotated."""
import anthropic

client = anthropic.Anthropic()
LONG_SYS = "x" * 5000

response = client.messages.create(
    model="claude-3-5-sonnet-20241022",
    max_tokens=1024,
    system=[
        {
            "type": "text",
            "text": LONG_SYS,
            "cache_control": {"type": "ephemeral"},
        }
    ],
    messages=[{"role": "user", "content": "Hi"}],
)
