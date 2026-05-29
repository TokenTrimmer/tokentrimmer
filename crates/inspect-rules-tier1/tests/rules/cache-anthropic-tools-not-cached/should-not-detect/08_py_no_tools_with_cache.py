import anthropic
client = anthropic.Anthropic()
resp = client.messages.create(
    model="claude-sonnet-4-6",
    max_tokens=1024,
    system=[{"type": "text", "text": "You are helpful.", "cache_control": {"type": "ephemeral"}}],
    messages=[{"role": "user", "content": "Hello."}],
)
